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
//! lines are validated first into a bounded temporary snapshot; a malformed
//! line aborts before any writes without retaining the whole file in memory.
//! Physical lines are capped at 96 MiB and the file at 512 MiB. Valid ops are
//! dispatched in chunks of at most 100 and 32 MiB (one larger op runs alone)
//! through the same
//! in-process runtime path (daemon fast-path is intentionally skipped for
//! bulk apply — the daemon is warm-state optimised, not throughput optimised).
//! A progress line is printed per chunk. `--save-file` streams ordered rows to
//! a sink whose final-file publication is atomic; the database chunks commit
//! incrementally. After dispatch begins, success and failure both print a
//! reconciliation manifest. An aborted manifest names confirmed committed
//! chunks and any dispatched chunk whose response could not be verified.
//! Without `--save-file`, validated row payloads are discarded after aggregation.
//! `--dry-run` validates every line and prints a per-verb summary without writes.

use std::collections::BTreeMap;
use std::io::{BufRead as _, Read as _, Seek as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;

#[cfg(test)]
use khive_mcp::serve::resolve_runtime_config;
use khive_mcp::serve::{
    apply_env_output_format, build_server_multi_backend_with_db_anchor, config_discovery_db_anchor,
    enforce_strict_actor_mode, install_resolved_blob_store,
    normalize_redundant_db_override_with_source, reject_conflicting_db_override_with_source,
    validate_declared_backend_access_modes, RuntimeConfigInputs,
};
use khive_mcp::server::KhiveMcpServer;
#[cfg(unix)]
use khive_mcp::server::{compute_config_id, compute_config_id_with_storage_mode};
use khive_mcp::tools::request::RequestParams;
#[cfg(unix)]
use khive_runtime::{daemon::PROTOCOL_VERSION, DaemonRequestFrame};
use khive_runtime::{KhiveConfig, KhiveRuntime, Namespace, RuntimeConfig};
use khive_types::RefusalReason;

/// Stable stderr prefix for machine-classifiable exec refusals.
const REFUSAL_PREFIX: &str = "kkernel-refusal: ";

#[derive(Debug)]
struct ExecRefusal {
    reason: RefusalReason,
    message: String,
}

impl std::fmt::Display for ExecRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExecRefusal {}

fn refusal_error(reason: RefusalReason, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ExecRefusal {
        reason,
        message: message.into(),
    })
}

fn emit_refusal(reason: RefusalReason) {
    eprintln!("{REFUSAL_PREFIX}{reason}");
}

fn refusal_envelope_for_tools(
    tools: Vec<String>,
    chain: bool,
    reason: RefusalReason,
    message: &str,
) -> serde_json::Value {
    debug_assert!(
        !tools.is_empty(),
        "per-operation refusal envelopes require at least one parsed operation"
    );
    let total = tools.len();
    let results: Vec<serde_json::Value> = tools
        .into_iter()
        .enumerate()
        .map(|(index, tool)| {
            if chain && index > 0 {
                serde_json::json!({
                    "ok": false,
                    "tool": tool,
                    "aborted": true,
                    "message": message,
                    "reason": reason.as_str(),
                })
            } else {
                serde_json::json!({
                    "ok": false,
                    "tool": tool,
                    "error": message,
                    "reason": reason.as_str(),
                })
            }
        })
        .collect();
    let aborted = if chain { total.saturating_sub(1) } else { 0 };
    let failed = total - aborted;
    serde_json::json!({
        "results": results,
        "summary": {
            "total": total,
            "succeeded": 0,
            "failed": failed,
            "aborted": aborted,
        },
        "status": "partial",
    })
}

/// Build the CLI's structured invocation-level error shape.
///
/// A failure that occurs before an operation can be identified must not be
/// represented as a fabricated per-op result. This mirrors the existing
/// database-override refusal shape and preserves ADR-016's parse-before-
/// envelope boundary: `results` exists only after a real operation list does.
fn invocation_refusal_envelope(reason: RefusalReason, message: &str) -> serde_json::Value {
    let code = if reason == RefusalReason::ParseError {
        "invalid_params"
    } else {
        "invocation_refused"
    };
    serde_json::json!({
        "error": {
            "code": code,
            "message": message,
            "reason": reason.as_str(),
        },
        "invocation": {"started": false},
    })
}

/// Emit the stable stderr token and structured invocation-level error for a
/// refusal that has no parsed operation list.
fn report_unscoped_refusal(reason: RefusalReason, message: impl Into<String>) -> anyhow::Error {
    let message = message.into();
    emit_refusal(reason);
    println!(
        "{}",
        serde_json::to_string(&invocation_refusal_envelope(reason, &message))
            .expect("invocation refusal envelope is serializable")
    );
    anyhow::anyhow!(message)
}

/// Emit a per-operation refusal envelope when the supplied DSL parses. If it
/// does not parse, retain the invocation-level boundary instead of inventing a
/// synthetic operation name.
fn report_invocation_refusal(
    raw_ops: Option<&str>,
    reason: RefusalReason,
    error: impl std::fmt::Display,
) -> anyhow::Error {
    let message = error.to_string();
    let (tools, chain) = raw_ops
        .and_then(|ops| khive_request::parse_request(ops).ok())
        .map(|parsed| {
            let chain = parsed.mode == khive_request::ExecutionMode::Chain;
            let tools: Vec<String> = parsed.ops.into_iter().map(|op| op.tool).collect();
            (tools, chain)
        })
        .unwrap_or_default();
    if tools.is_empty() {
        report_unscoped_refusal(reason, message)
    } else {
        report_tools_refusal(tools, chain, reason, message)
    }
}

fn report_tools_refusal(
    tools: Vec<String>,
    chain: bool,
    reason: RefusalReason,
    message: impl Into<String>,
) -> anyhow::Error {
    let message = message.into();
    if tools.is_empty() {
        return report_unscoped_refusal(reason, message);
    }
    emit_refusal(reason);
    let envelope = refusal_envelope_for_tools(tools, chain, reason, &message);
    println!(
        "{}",
        serde_json::to_string(&envelope).expect("refusal envelope is serializable")
    );
    anyhow::anyhow!(message)
}

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
type ForwardFnPtr =
    for<'a> fn(&'a DaemonRequestFrame, Option<PathBuf>, Option<&'a str>) -> ForwardFuture<'a>;

/// Adapts the real `forward_or_spawn` to the `ForwardFnPtr` signature.
#[cfg(unix)]
fn forward_or_spawn_boxed<'a>(
    frame: &'a DaemonRequestFrame,
    config: Option<PathBuf>,
    db: Option<&'a str>,
) -> ForwardFuture<'a> {
    Box::pin(async move {
        khive_mcp::daemon::forward_or_spawn_with_config(frame, config.as_deref(), db).await
    })
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

/// Large payloads reduce the op count per dispatch so a 100-op chunk cannot
/// duplicate/stringify the entire accepted input snapshot at once. One op may
/// exceed this budget (up to the physical-line ceiling) and runs alone.
const OPS_FILE_CHUNK_MAX_BYTES: usize = 32 * 1024 * 1024;

/// A single JSONL op may carry one 64 MiB Moodboard object as base64 plus its
/// bounded metadata, but cannot grow without limit before JSON validation.
const MAX_OPS_FILE_LINE_BYTES: usize = 96 * 1024 * 1024;

/// The validated on-disk snapshot bounds both disk amplification and the
/// all-in-memory atomic path. Non-atomic execution retains only one chunk.
const MAX_OPS_FILE_BYTES: u64 = 512 * 1024 * 1024;

const MAX_OPS_FILE_FAILURE_DETAILS: usize = 1_000;
const MAX_OPS_FILE_FAILURE_ERROR_BYTES: usize = 4 * 1024;

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

    /// Explicit khive configuration file. This selects the same engine,
    /// backend-topology, and actor configuration used by `kkernel mcp`.
    #[arg(long, env = "KHIVE_CONFIG")]
    pub config: Option<PathBuf>,

    /// Namespace to operate in.
    #[arg(long, default_value = "local")]
    pub namespace: String,

    /// Pin the acting identity for this invocation.
    ///
    /// This is an attribution and authorization identity, not a storage
    /// namespace. It has higher precedence than project config and
    /// `KHIVE_ACTOR`, and is checked by the same dispatch gate as every other
    /// resolved actor. A refused actor is never retried as a fallback identity.
    #[arg(long, value_name = "ACTOR", conflicts_with = "pending_events")]
    pub actor: Option<String>,

    /// Require the resolved acting identity to equal this value.
    ///
    /// Composes with `--actor`; without it, validates the normal project,
    /// config, and environment resolution chain. Use `local` to require the
    /// anonymous/local identity. A mismatch fails before dispatch.
    #[arg(long, value_name = "ACTOR", conflicts_with = "pending_events")]
    pub expect_actor: Option<String>,

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
    ///
    /// The legacy `--ops-file` path without `--save-file` keeps its established
    /// aggregate JSON summary and does not forward this override to transient
    /// rows. Combined bulk save always persists lossless JSON rows, matching
    /// inline save.
    #[arg(long, value_name = "FORMAT")]
    pub output_format: Option<String>,

    /// Verbose output: print per-event progress to stderr.
    #[arg(long, short = 'v')]
    pub verbose: bool,

    /// Write results as JSONL to this path and print a self-describing manifest.
    ///
    /// The manifest (`{path, rows, per_column_null_counts, schema_fingerprint,
    /// checksum, summary, failures?}`) is printed to stdout instead of the raw
    /// results. Optional `failures` entries project each failed row's error and
    /// any stable reason. With `--ops-file`, ordered per-op envelopes from every
    /// chunk are retained in one JSONL file. Database chunks commit incrementally;
    /// after dispatch begins every exit prints a reconciliation manifest. A
    /// post-dispatch failure prints `status="aborted"`, the confirmed committed
    /// chunks, and any dispatched-but-unverified chunk. Its incomplete temp file
    /// is discarded, so a prior destination remains unchanged. Parent directories
    /// are created if absent.
    ///
    /// Note: `--save-file` always runs in-process and bypasses the warm daemon,
    /// so ANN-dependent verbs (e.g. `knowledge.suggest`, `knowledge.compose`) may
    /// hit a cold or warming index on the first call after a daemon restart.
    ///
    /// Example:
    ///   kkernel exec 'list(kind="entity")' --save-file /tmp/entities.jsonl
    #[arg(long, conflicts_with = "dry_run")]
    pub save_file: Option<String>,

    /// JSONL file of ops to apply in bulk.
    ///
    /// Each non-blank line must be a JSON object `{"tool":"verb","args":{...}}`
    /// (the same JSON form the MCP `request` tool accepts).  All lines are
    /// parsed before any write.  A malformed line prints the line number and
    /// error, then aborts without writing.
    ///
    /// The source is capped at 512 MiB total and 96 MiB per physical line, then
    /// spooled to a validated temporary snapshot before writes. Dispatch chunks
    /// are capped at 100 ops and 32 MiB (one larger op runs alone). Progress is
    /// printed per chunk to stderr; the final aggregate summary is printed to
    /// stdout, or `--save-file` incrementally writes ordered JSONL rows to a
    /// sibling temp file. Success atomically publishes the complete file and
    /// prints its ordinary manifest. A later failure leaves database effects
    /// incremental, discards the incomplete temp file, and prints an aborted
    /// reconciliation manifest before returning non-zero.
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
    /// when any applied op fails). Without this flag a *partially* failed
    /// batch still exits 0 — the per-op `results` entries and the
    /// `summary`/`status` fields in the printed output are the signal
    /// (#1220). A batch in which *every* op failed always exits non-zero,
    /// with or without this flag (#1339). With `--atomic`, this flag does not
    /// change the established atomic exit semantics, but it does annotate
    /// otherwise-unclassified not-committed result rows with the stable
    /// `strict-op-failure` reason.
    #[arg(long)]
    pub strict: bool,
}

/// A single parsed op entry from an ops-file line.
#[derive(Debug, Clone, serde::Serialize)]
pub(crate) struct OpsFileEntry {
    pub(crate) tool: String,
    pub(crate) args: serde_json::Value,
}

#[derive(Debug)]
struct ValidatedOpsFile {
    snapshot: std::fs::File,
    total: usize,
    per_verb: BTreeMap<String, usize>,
}

fn parse_ops_file_line(raw: &str, line_num: usize) -> Result<Option<OpsFileEntry>> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }

    let obj: serde_json::Value = serde_json::from_str(trimmed).map_err(|error| {
        refusal_error(
            RefusalReason::ParseError,
            format!("ops-file line {line_num}: invalid JSON: {error}"),
        )
    })?;
    let obj = obj.as_object().ok_or_else(|| {
        refusal_error(
            RefusalReason::ParseError,
            format!(
                "ops-file line {line_num}: expected a JSON object \
                 {{\"tool\":...,\"args\":...}}, got a non-object value"
            ),
        )
    })?;
    let tool = obj
        .get("tool")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            refusal_error(
                RefusalReason::ParseError,
                format!("ops-file line {line_num}: missing or non-string \"tool\" field"),
            )
        })?
        .to_owned();
    let args = match obj.get("args") {
        None => serde_json::Value::Object(serde_json::Map::new()),
        Some(v) if v.is_object() => v.clone(),
        Some(v) => {
            return Err(refusal_error(
                RefusalReason::ParseError,
                format!("ops-file line {line_num}: \"args\" must be a JSON object, got {v}"),
            ))
        }
    };
    Ok(Some(OpsFileEntry { tool, args }))
}

fn read_bounded_ops_line<R: std::io::BufRead>(
    reader: &mut R,
    line_num: usize,
) -> Result<Option<String>> {
    read_bounded_ops_line_with_limit(reader, line_num, MAX_OPS_FILE_LINE_BYTES)
}

fn read_bounded_ops_line_with_limit<R: std::io::BufRead>(
    reader: &mut R,
    line_num: usize,
    limit: usize,
) -> Result<Option<String>> {
    let mut bytes = Vec::new();
    let read = {
        let mut limited = (&mut *reader).take((limit + 1) as u64);
        limited
            .read_until(b'\n', &mut bytes)
            .with_context(|| format!("read ops-file line {line_num}"))?
    };
    if read == 0 {
        return Ok(None);
    }
    if read > limit {
        anyhow::bail!("ops-file line {line_num} exceeds the {limit}-byte physical-line limit");
    }
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
        if bytes.last() == Some(&b'\r') {
            bytes.pop();
        }
    }
    String::from_utf8(bytes)
        .map(Some)
        .map_err(|error| anyhow::anyhow!("ops-file line {line_num} is not valid UTF-8: {error}"))
}

/// Validate the complete source before runtime construction or writes, while
/// spooling a stable bounded snapshot instead of retaining all argument Values.
fn validate_ops_file(path: &Path) -> Result<ValidatedOpsFile> {
    let file =
        std::fs::File::open(path).with_context(|| format!("open ops-file {}", path.display()))?;
    let metadata_len = file
        .metadata()
        .with_context(|| format!("stat ops-file {}", path.display()))?
        .len();
    if metadata_len > MAX_OPS_FILE_BYTES {
        anyhow::bail!(
            "ops-file {} is {metadata_len} bytes, exceeding the {MAX_OPS_FILE_BYTES}-byte total limit",
            path.display()
        );
    }
    let mut reader = std::io::BufReader::new(file);
    let mut snapshot = tempfile::tempfile().context("create validated ops-file snapshot")?;
    let mut total = 0_usize;
    let mut total_bytes = 0_u64;
    let mut per_verb = BTreeMap::new();
    let mut line_num = 1_usize;
    while let Some(raw) = read_bounded_ops_line(&mut reader, line_num)? {
        total_bytes = total_bytes
            .checked_add(raw.len() as u64)
            .and_then(|value| value.checked_add(1))
            .ok_or_else(|| anyhow::anyhow!("ops-file byte count overflow"))?;
        if total_bytes > MAX_OPS_FILE_BYTES {
            anyhow::bail!(
                "ops-file exceeds the {MAX_OPS_FILE_BYTES}-byte total limit while reading line {line_num}"
            );
        }
        if let Some(op) = parse_ops_file_line(&raw, line_num)? {
            *per_verb.entry(op.tool).or_insert(0) += 1;
            snapshot
                .write_all(raw.trim().as_bytes())
                .context("write validated ops-file snapshot")?;
            snapshot
                .write_all(b"\n")
                .context("write validated ops-file snapshot newline")?;
            total += 1;
        }
        line_num += 1;
    }
    snapshot
        .rewind()
        .context("rewind validated ops-file snapshot")?;
    Ok(ValidatedOpsFile {
        snapshot,
        total,
        per_verb,
    })
}

fn parse_validated_snapshot<R>(snapshot: &mut R) -> Result<Vec<OpsFileEntry>>
where
    R: std::io::Read + std::io::Seek,
{
    snapshot
        .rewind()
        .context("rewind validated ops-file snapshot")?;
    let mut reader = std::io::BufReader::new(snapshot);
    let mut ops = Vec::new();
    let mut line_num = 1_usize;
    while let Some(raw) = read_bounded_ops_line(&mut reader, line_num)? {
        if let Some(op) = parse_ops_file_line(&raw, line_num)? {
            ops.push(op);
        }
        line_num += 1;
    }
    Ok(ops)
}

fn validated_tool_names<R>(snapshot: &mut R) -> Result<Vec<String>>
where
    R: std::io::Read + std::io::Seek,
{
    snapshot
        .rewind()
        .context("rewind validated ops-file snapshot")?;
    let mut reader = std::io::BufReader::new(snapshot);
    let mut tools = Vec::new();
    let mut line_num = 1_usize;
    while let Some(raw) = read_bounded_ops_line(&mut reader, line_num)? {
        if let Some(op) = parse_ops_file_line(&raw, line_num)? {
            tools.push(op.tool);
        }
        line_num += 1;
    }
    Ok(tools)
}

/// Enforce the atomic operation ceiling before the validated snapshot is
/// parsed into owned JSON values. The second guard in `atomic_apply` remains
/// defense in depth for callers that bypass this CLI transport seam.
fn parse_atomic_validated_snapshot<R>(
    snapshot: &mut R,
    total: usize,
    max_ops: usize,
) -> Result<Vec<OpsFileEntry>>
where
    R: std::io::Read + std::io::Seek,
{
    if total > max_ops {
        anyhow::bail!(
            "--atomic op count {total} exceeds the configured maximum {max_ops}; \
             split the file or raise --atomic-max-ops"
        );
    }
    parse_validated_snapshot(snapshot)
}

/// Parse a JSONL ops-file.
///
/// Returns the ordered list of ops, or an error naming the first malformed
/// line.  Blank lines are skipped.
///
/// Each line must be a JSON object `{"tool":"verb","args":{...}}`.  `"args"`
/// is optional and defaults to an empty object.  Any other top-level keys are
/// silently ignored so the format is forward-compatible.
#[cfg(test)]
pub(crate) fn parse_ops_file(path: &Path) -> Result<Vec<OpsFileEntry>> {
    let mut validated = validate_ops_file(path)?;
    parse_validated_snapshot(&mut validated.snapshot)
}

/// Extract the failed entries of one dispatched chunk as `{op_index, tool,
/// error, reason?}` objects, with `op_index` global across chunks. A failure summary
/// without the per-op reason strings is unactionable: a gate rejection, a
/// schema error, and a transient failure all look identical, and pipelines
/// that trust the counts alone lose records silently.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpsFileReportMode {
    /// Preserve the pre-save-file CLI wire exactly for compatibility.
    LegacyNoSave,
    /// Bound durable manifest diagnostics independently from saved rows.
    BoundedSave,
}

fn collect_op_failures(
    parsed: &serde_json::Value,
    applied_before: usize,
    mode: OpsFileReportMode,
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
                other if mode == OpsFileReportMode::BoundedSave => bounded_failure_error(other),
                other => other.clone(),
            };
            let mut failure = serde_json::json!({
                "op_index": applied_before + i,
                "tool": entry["tool"].as_str().unwrap_or("?"),
                "error": error,
            });
            if mode == OpsFileReportMode::BoundedSave {
                failure["aborted"] =
                    serde_json::Value::Bool(entry["aborted"].as_bool().unwrap_or(false));
                if let Some(reason) = entry["reason"].as_str().and_then(RefusalReason::from_token) {
                    failure["reason"] = serde_json::json!(reason.as_str());
                }
            }
            failure
        })
        .collect()
}

fn retain_failure_detail(
    mode: OpsFileReportMode,
    failure: serde_json::Value,
    failures: &mut Vec<serde_json::Value>,
    omitted: &mut usize,
) -> bool {
    if mode == OpsFileReportMode::BoundedSave && failures.len() >= MAX_OPS_FILE_FAILURE_DETAILS {
        *omitted += 1;
        false
    } else {
        failures.push(failure);
        true
    }
}

fn ops_file_progress_line(
    mode: OpsFileReportMode,
    applied: usize,
    total: usize,
    succeeded: usize,
    failed: usize,
    aborted: usize,
) -> String {
    match mode {
        OpsFileReportMode::LegacyNoSave => {
            format!("applied {applied}/{total} (ok={succeeded}, failed={failed})")
        }
        OpsFileReportMode::BoundedSave => format!(
            "applied {applied}/{total} (ok={succeeded}, failed={failed}, aborted={aborted})"
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn ops_file_summary(
    mode: OpsFileReportMode,
    total: usize,
    succeeded: usize,
    failed: usize,
    aborted: usize,
    failures: Vec<serde_json::Value>,
    failure_details_omitted: usize,
) -> serde_json::Value {
    let mut summary = match mode {
        OpsFileReportMode::LegacyNoSave => serde_json::json!({
            "total": total,
            "succeeded": succeeded,
            "failed": failed,
        }),
        OpsFileReportMode::BoundedSave => serde_json::json!({
            "total": total,
            "succeeded": succeeded,
            "failed": failed,
            "aborted": aborted,
        }),
    };
    if !failures.is_empty() {
        summary["failures"] = serde_json::Value::Array(failures);
    }
    if mode == OpsFileReportMode::BoundedSave && failure_details_omitted > 0 {
        summary["failure_details_omitted"] = serde_json::json!(failure_details_omitted);
    }
    summary
}

fn bounded_failure_error(error: &serde_json::Value) -> serde_json::Value {
    let mut writer = CountingWriter::default();
    if serde_json::to_writer(&mut writer, error).is_ok()
        && writer.bytes <= MAX_OPS_FILE_FAILURE_ERROR_BYTES
    {
        error.clone()
    } else {
        serde_json::Value::String(format!(
            "error detail omitted: exceeds {MAX_OPS_FILE_FAILURE_ERROR_BYTES}-byte ops-file diagnostic limit"
        ))
    }
}

fn required_summary_count(parsed: &serde_json::Value, field: &str) -> Result<usize> {
    let value = parsed
        .pointer(&format!("/summary/{field}"))
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| anyhow::anyhow!("dispatch result is missing integer summary.{field}"))?;
    usize::try_from(value).context("dispatch summary count does not fit usize")
}

fn classify_ordered_chunk(
    chunk: &[OpsFileEntry],
    results: &[serde_json::Value],
) -> Result<(usize, usize, usize)> {
    if results.len() != chunk.len() {
        anyhow::bail!(
            "ordered chunk result count {} does not match input count {}",
            results.len(),
            chunk.len()
        );
    }
    let mut succeeded = 0_usize;
    let mut failed = 0_usize;
    let mut aborted = 0_usize;
    for (index, (op, row)) in chunk.iter().zip(results).enumerate() {
        let object = row
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("dispatch result row {index} is not a JSON object"))?;
        let returned_tool = object
            .get("tool")
            .and_then(serde_json::Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("dispatch result row {index} has no string tool"))?;
        if returned_tool != op.tool {
            anyhow::bail!(
                "dispatch result row {index} tool mismatch: expected {:?}, got {:?}",
                op.tool,
                returned_tool
            );
        }
        let ok = object
            .get("ok")
            .and_then(serde_json::Value::as_bool)
            .ok_or_else(|| anyhow::anyhow!("dispatch result row {index} has no boolean ok"))?;
        let row_aborted = match object.get("aborted") {
            None => false,
            Some(value) => value.as_bool().ok_or_else(|| {
                anyhow::anyhow!("dispatch result row {index} has non-boolean aborted")
            })?,
        };
        match (ok, row_aborted) {
            (true, false) => {
                if !object.contains_key("result") {
                    anyhow::bail!(
                        "dispatch result row {index} is successful but has no result field"
                    );
                }
                if object.contains_key("error") {
                    anyhow::bail!(
                        "dispatch result row {index} is successful but also has an error field"
                    );
                }
                succeeded += 1;
            }
            (false, false) => {
                if !object.contains_key("error") {
                    anyhow::bail!("dispatch result row {index} failed but has no error field");
                }
                if object.contains_key("result") {
                    anyhow::bail!("dispatch result row {index} failed but also has a result field");
                }
                failed += 1;
            }
            (false, true) => {
                if !object.contains_key("error") {
                    anyhow::bail!("dispatch result row {index} aborted but has no error field");
                }
                if object.contains_key("result") {
                    anyhow::bail!(
                        "dispatch result row {index} aborted but also has a result field"
                    );
                }
                aborted += 1;
            }
            (true, true) => {
                anyhow::bail!("dispatch result row {index} cannot be both successful and aborted")
            }
        }
    }
    Ok((succeeded, failed, aborted))
}

fn validate_ordered_chunk_envelope(
    chunk: &[OpsFileEntry],
    parsed: &serde_json::Value,
    chunk_number: usize,
) -> Result<(usize, usize, usize)> {
    let results = parsed
        .get("results")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| {
            anyhow::anyhow!("dispatch chunk {chunk_number} returned no results array")
        })?;
    let chunk_total = required_summary_count(parsed, "total")?;
    let chunk_succeeded = required_summary_count(parsed, "succeeded")?;
    let chunk_failed = required_summary_count(parsed, "failed")?;
    let chunk_aborted = required_summary_count(parsed, "aborted")?;
    let (derived_succeeded, derived_failed, derived_aborted) =
        classify_ordered_chunk(chunk, results)?;
    if chunk_total != chunk.len()
        || chunk_succeeded != derived_succeeded
        || chunk_failed != derived_failed
        || chunk_aborted != derived_aborted
    {
        anyhow::bail!(
            "dispatch chunk {chunk_number} summary disagrees with ordered rows: expected total {}, summary total {}, derived/summary succeeded {derived_succeeded}/{chunk_succeeded}, failed {derived_failed}/{chunk_failed}, aborted {derived_aborted}/{chunk_aborted}",
            chunk.len(),
            chunk_total,
        );
    }
    let status = parsed
        .get("status")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            anyhow::anyhow!("dispatch chunk {chunk_number} returned no string status")
        })?;
    let expected_status = if derived_failed == 0 && derived_aborted == 0 {
        "success"
    } else {
        "partial"
    };
    if status != expected_status {
        anyhow::bail!(
            "dispatch chunk {chunk_number} status disagrees with ordered rows: expected {expected_status:?}, got {status:?}"
        );
    }
    Ok((chunk_succeeded, chunk_failed, chunk_aborted))
}

fn should_defer_chunk_entry(current_count: usize, current_bytes: usize, next_bytes: usize) -> bool {
    current_count > 0
        && (current_count >= OPS_FILE_CHUNK_SIZE
            || current_bytes.saturating_add(next_bytes) > OPS_FILE_CHUNK_MAX_BYTES)
}

#[derive(Default)]
struct CountingWriter {
    bytes: usize,
}

impl std::io::Write for CountingWriter {
    fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
        self.bytes = self.bytes.checked_add(buffer.len()).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::FileTooLarge, "JSON byte count overflow")
        })?;
        Ok(buffer.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(Debug)]
struct AbortedOpsFileError {
    message: String,
    #[cfg_attr(not(test), allow(dead_code))]
    manifest: serde_json::Value,
}

impl std::fmt::Display for AbortedOpsFileError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for AbortedOpsFileError {}

#[allow(clippy::too_many_arguments)]
fn emit_aborted_ops_file_manifest(
    error: anyhow::Error,
    save_path: &str,
    requested_total: usize,
    confirmed_ops: usize,
    committed_chunks: &[usize],
    dispatched_chunk: Option<usize>,
    summary: serde_json::Value,
) -> anyhow::Error {
    let message = format!("{error:#}");
    let mut manifest = serde_json::json!({
        "status": "aborted",
        "path": save_path,
        "file_published": false,
        "requested_total": requested_total,
        "confirmed_ops": confirmed_ops,
        "unconfirmed_ops": requested_total.saturating_sub(confirmed_ops),
        "committed_chunks": committed_chunks,
        "summary": summary,
        "error": message.clone(),
    });
    if let Some(chunk_number) = dispatched_chunk {
        manifest["dispatched_chunk"] = serde_json::json!(chunk_number);
    }
    println!(
        "{}",
        serde_json::to_string(&manifest).expect("serialize aborted ops-file manifest")
    );
    anyhow::Error::new(AbortedOpsFileError { message, manifest })
}

/// Apply a parsed ops-file against the given server, printing progress to
/// stderr and either the final summary or a success/aborted save manifest.
#[allow(clippy::too_many_arguments)]
async fn apply_ops_file_reader<R: std::io::BufRead>(
    server: &KhiveMcpServer,
    reader: R,
    total: usize,
    presentation: Option<String>,
    _output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
) -> Result<serde_json::Value> {
    apply_ops_file_reader_with_response_transform(
        server,
        reader,
        total,
        presentation,
        _output_format,
        save_file,
        strict,
        |_, raw| raw,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn apply_ops_file_reader_with_response_transform<R, F>(
    server: &KhiveMcpServer,
    mut reader: R,
    total: usize,
    presentation: Option<String>,
    _output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
    mut response_transform: F,
) -> Result<serde_json::Value>
where
    R: std::io::BufRead,
    F: FnMut(usize, String) -> String,
{
    let report_mode = if save_file.is_some() {
        OpsFileReportMode::BoundedSave
    } else {
        OpsFileReportMode::LegacyNoSave
    };
    let mut total_succeeded: usize = 0;
    let mut total_failed: usize = 0;
    let mut total_aborted: usize = 0;
    let mut failures: Vec<serde_json::Value> = Vec::new();
    let mut failure_details_omitted = 0_usize;
    // Preflight the destination before the first chunk can commit. Rows then
    // stream to its sibling temp file. Success publishes it atomically; after
    // dispatch begins, failure drops it and emits a reconciliation manifest.
    let save_path = save_file.clone();
    let mut save_sink = save_file
        .as_deref()
        .map(|path| khive_mcp::save_sink::JsonlSaveSink::new(Path::new(path), false))
        .transpose()?;
    let mut processed = 0_usize;
    let mut snapshot_line = 1_usize;
    let mut chunk_idx = 0_usize;
    let mut eof = false;
    let mut pending: Option<(OpsFileEntry, usize)> = None;
    let mut confirmed_ops = 0_usize;
    let mut committed_chunks = Vec::new();
    let mut dispatched_chunk = None;

    let execution_result: Result<()> = async {
        while !eof {
            let mut chunk = Vec::with_capacity(OPS_FILE_CHUNK_SIZE);
            let mut chunk_bytes = 0_usize;
            if let Some((op, bytes)) = pending.take() {
                chunk_bytes = bytes;
                chunk.push(op);
            }
            while chunk.len() < OPS_FILE_CHUNK_SIZE {
                let Some(raw) = read_bounded_ops_line(&mut reader, snapshot_line)? else {
                    eof = true;
                    break;
                };
                let physical_bytes = raw.len().saturating_add(1);
                if let Some(op) = parse_ops_file_line(&raw, snapshot_line)? {
                    if should_defer_chunk_entry(chunk.len(), chunk_bytes, physical_bytes) {
                        pending = Some((op, physical_bytes));
                        snapshot_line += 1;
                        break;
                    }
                    chunk_bytes = chunk_bytes.saturating_add(physical_bytes);
                    chunk.push(op);
                }
                snapshot_line += 1;
            }
            if chunk.is_empty() {
                break;
            }
            let applied_before = processed;

            // Serialize the typed entries directly; avoid a second Value tree that
            // would clone every base64 argument before producing the request text.
            let batch_json = serde_json::to_string(&chunk).context("serialize chunk to JSON")?;

            let params = RequestParams {
                ops: batch_json,
                presentation: presentation.clone(),
                presentation_per_op: None,
                save_to: None,
                // Inline --save-file writes raw results before format rendering.
                // Reproduce that lossless shape for the combined bulk save. The
                // no-save path deliberately preserves its pre-PR behavior, which
                // did not forward the CLI output-format override to each chunk.
                format: if save_sink.is_some() {
                    Some("json".to_string())
                } else {
                    None
                },
                format_per_op: None,
                request_id: None,
            };

            let chunk_number = chunk_idx + 1;
            dispatched_chunk = Some(chunk_number);
            let raw = server
                .dispatch_request_local(params)
                .await
                .map_err(|e| anyhow::anyhow!("dispatch chunk {chunk_number}: {e}"))?;
            let raw = response_transform(chunk_number, raw);

            let mut parsed: serde_json::Value =
                serde_json::from_str(&raw).context("parse dispatch result")?;
            annotate_and_emit_refusals(&mut parsed, strict);
            let (chunk_succeeded, chunk_failed, chunk_aborted) =
                validate_ordered_chunk_envelope(&chunk, &parsed, chunk_number)?;
            let chunk_results = parsed["results"]
                .as_array()
                .expect("validated ordered results array");

            total_succeeded += chunk_succeeded;
            total_failed += chunk_failed;
            total_aborted += chunk_aborted;
            confirmed_ops += chunk.len();
            committed_chunks.push(chunk_number);
            dispatched_chunk = None;

            for failure in collect_op_failures(&parsed, applied_before, report_mode) {
                let reason = match &failure["error"] {
                    serde_json::Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                let op_index = failure["op_index"].clone();
                let tool = failure["tool"].as_str().unwrap_or("?").to_string();
                if retain_failure_detail(
                    report_mode,
                    failure,
                    &mut failures,
                    &mut failure_details_omitted,
                ) {
                    eprintln!("op {} ({}) failed: {reason}", op_index, tool,);
                }
            }

            if let Some(save_sink) = save_sink.as_mut() {
                for row in chunk_results {
                    save_sink.write_row(row)?;
                }
            }

            processed += chunk.len();
            let applied_now = processed;
            eprintln!(
                "{}",
                ops_file_progress_line(
                    report_mode,
                    applied_now,
                    total,
                    total_succeeded,
                    total_failed,
                    total_aborted,
                )
            );
            chunk_idx += 1;
        }
        if processed != total {
            anyhow::bail!(
                "validated ops-file snapshot changed: expected {total} ops, read {}",
                processed
            );
        }
        Ok(())
    }
    .await;

    if let Err(error) = execution_result {
        if let Some(path) = save_path
            .as_deref()
            .filter(|_| !committed_chunks.is_empty() || dispatched_chunk.is_some())
        {
            drop(save_sink.take());
            let summary = ops_file_summary(
                report_mode,
                confirmed_ops,
                total_succeeded,
                total_failed,
                total_aborted,
                failures,
                failure_details_omitted,
            );
            return Err(emit_aborted_ops_file_manifest(
                error,
                path,
                total,
                confirmed_ops,
                &committed_chunks,
                dispatched_chunk,
                summary,
            ));
        }
        return Err(error);
    }

    let summary = ops_file_summary(
        report_mode,
        total,
        total_succeeded,
        total_failed,
        total_aborted,
        failures,
        failure_details_omitted,
    );
    let output = if let Some(save_sink) = save_sink {
        let manifest = match save_sink.finish(summary.clone()) {
            Ok(manifest) => manifest,
            Err(error) => {
                let path = save_path
                    .as_deref()
                    .expect("save sink exists only when save path exists");
                return Err(emit_aborted_ops_file_manifest(
                    error,
                    path,
                    total,
                    confirmed_ops,
                    &committed_chunks,
                    None,
                    summary,
                ));
            }
        };
        println!(
            "{}",
            serde_json::to_string(&manifest).expect("serialize save manifest")
        );
        manifest
    } else {
        println!(
            "{}",
            serde_json::to_string_pretty(&summary).expect("serialize summary")
        );
        summary
    };
    if total > 0 && total_succeeded == 0 {
        match report_mode {
            OpsFileReportMode::LegacyNoSave => anyhow::bail!(
                "every op failed: {total_failed} op(s) failed out of {total}, 0 succeeded (see printed summary above)"
            ),
            OpsFileReportMode::BoundedSave => anyhow::bail!(
                "every op failed: {total_failed} failed, {total_aborted} aborted out of {total}, 0 succeeded (see printed output above)"
            ),
        }
    }
    if strict {
        match report_mode {
            OpsFileReportMode::LegacyNoSave if total_failed > 0 => anyhow::bail!(
                "--strict: {total_failed} op(s) failed out of {total} (see printed summary above)"
            ),
            OpsFileReportMode::BoundedSave if total_failed > 0 || total_aborted > 0 => {
                anyhow::bail!(
                    "--strict: {total_failed} op(s) failed, {total_aborted} op(s) aborted out of {total} (see printed output above)"
                )
            }
            _ => {}
        }
    }
    Ok(output)
}

#[cfg(test)]
async fn apply_ops_file(
    server: &KhiveMcpServer,
    ops: Vec<OpsFileEntry>,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
) -> Result<serde_json::Value> {
    let total = ops.len();
    let mut encoded = Vec::new();
    for op in ops {
        serde_json::to_writer(
            &mut encoded,
            &serde_json::json!({"tool": op.tool, "args": op.args}),
        )
        .context("serialize test ops-file entry")?;
        encoded.push(b'\n');
    }
    apply_ops_file_reader(
        server,
        std::io::Cursor::new(encoded),
        total,
        presentation,
        output_format,
        save_file,
        strict,
    )
    .await
}

#[cfg(test)]
#[allow(clippy::too_many_arguments)]
async fn apply_ops_file_with_response_transform<F>(
    server: &KhiveMcpServer,
    ops: Vec<OpsFileEntry>,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    strict: bool,
    response_transform: F,
) -> Result<serde_json::Value>
where
    F: FnMut(usize, String) -> String,
{
    let total = ops.len();
    let mut encoded = Vec::new();
    for op in ops {
        serde_json::to_writer(
            &mut encoded,
            &serde_json::json!({"tool": op.tool, "args": op.args}),
        )
        .context("serialize test ops-file entry")?;
        encoded.push(b'\n');
    }
    apply_ops_file_reader_with_response_transform(
        server,
        std::io::Cursor::new(encoded),
        total,
        presentation,
        output_format,
        save_file,
        strict,
        response_transform,
    )
    .await
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

    // Parsing is the invocation boundary, before identity/configuration guards
    // choose a competing refusal. This makes malformed inline DSL and malformed
    // JSONL deterministically report `parse-error` regardless of whether strict
    // actor mode or `--expect-actor` would also reject a valid invocation.
    preflight_exec_mode(&mode)?;

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
    let (mut cfg, db_anchor) =
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

    // Apply the explicit actor only AFTER the shared resolver has loaded the
    // project/config/environment fallbacks. This makes the CLI value the true
    // highest-precedence tier without coupling identity to storage namespace.
    // Keeping the selected identity in `cfg` also sends every execution mode
    // through its existing gate seam: daemon request identity, local registry
    // dispatch, ops-file dispatch, or atomic apply's pre-write authorization.
    if let Err(error) = apply_actor_pin_and_expectation(
        &mut cfg,
        args.actor.as_deref(),
        args.expect_actor.as_deref(),
    ) {
        if let Some(refusal) = error.downcast_ref::<ExecRefusal>() {
            return Err(report_mode_refusal(&mode, refusal.reason, &refusal.message));
        }
        return Err(error);
    }

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
                args.output_format,
                args.save_file,
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

/// Apply the explicit exec actor tier and validate an optional identity
/// expectation before any daemon forwarding or local dispatch occurs.
fn apply_actor_pin_and_expectation(
    cfg: &mut RuntimeConfig,
    actor: Option<&str>,
    expect_actor: Option<&str>,
) -> Result<()> {
    if let Some(raw) = actor {
        let parsed =
            Namespace::parse(raw).map_err(|e| anyhow::anyhow!("invalid --actor {raw:?}: {e}"))?;

        // The resolver may have already folded the displaced actor (project
        // `[actor] id`, `KHIVE_ACTOR`, etc.) into the default read visible-set
        // (ADR-007 Rev 4 Rule 3b). Drop exactly that entry before pinning, so
        // the new identity's default reads don't keep exposing the actor it
        // replaced; any other explicitly configured `visible_namespaces` entry
        // is untouched.
        if let Some(prev) = cfg.actor_id.as_deref() {
            if let Ok(prev_ns) = Namespace::parse(prev) {
                cfg.visible_namespaces.retain(|ns| *ns != prev_ns);
            }
        }

        cfg.actor_id = if parsed == Namespace::local() {
            None
        } else {
            if !cfg.visible_namespaces.contains(&parsed) {
                cfg.visible_namespaces.push(parsed.clone());
            }
            Some(parsed.as_str().to_owned())
        };
    }

    if let Some(raw_expected) = expect_actor {
        let expected = Namespace::parse(raw_expected)
            .map_err(|e| anyhow::anyhow!("invalid --expect-actor {raw_expected:?}: {e}"))?;
        let actual = khive_runtime::resolve_actor(cfg.actor_id.as_deref());
        if actual.id != expected.as_str() {
            return Err(refusal_error(
                RefusalReason::ExpectActorMismatch,
                format!(
                    "--expect-actor mismatch: expected {:?}, resolved {:?}",
                    expected.as_str(),
                    actual.id
                ),
            ));
        }
    }

    Ok(())
}

/// Decides the process exit code from the response envelope's `summary`.
/// `raw` must be the exact envelope string already printed to stdout — the
/// caller prints first, unconditionally, then this decides the exit code; a
/// caller piping the output still sees the full result either way.
///
/// Two tiers (#1220, #1339):
/// - Always: `Err` when the batch had ops and none succeeded. A fully-failed
///   invocation has no success to report; scripted single-op callers (the
///   dominant `exec` shape) check the process exit code, and exiting 0 there
///   converts loud op-level rejections into silent drops.
/// - `--strict` only: `Err` when any op failed or aborted (partial failure).
fn enforce_strict_batch_result(raw: &str, strict: bool) -> Result<()> {
    let Ok(parsed) = serde_json::from_str::<serde_json::Value>(raw) else {
        // Non-JSON output (e.g. --output-format table/auto): nothing to
        // inspect here. Exit-code enforcement only applies to the default
        // JSON shape.
        return Ok(());
    };
    let succeeded = parsed["summary"]["succeeded"].as_u64().unwrap_or(0);
    let failed = parsed
        .get("summary")
        .and_then(|summary| summary.get("failed"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    let aborted = parsed
        .get("summary")
        .and_then(|summary| summary.get("aborted"))
        .and_then(serde_json::Value::as_u64)
        .unwrap_or(0);
    if succeeded == 0 && (failed > 0 || aborted > 0) {
        anyhow::bail!(
            "every op failed: {failed} failed, {aborted} aborted, 0 succeeded (see printed output above)"
        );
    }
    if strict && (failed > 0 || aborted > 0) {
        anyhow::bail!(
            "--strict: {failed} op(s) failed, {aborted} op(s) aborted (see printed output above)"
        );
    }
    Ok(())
}

/// Emit stable classifications already attached by the dispatch layer. Under
/// `--strict`, otherwise-unclassified failed or aborted entries receive
/// `strict-op-failure`; a more specific server-owned reason always wins.
/// Returns whether the JSON value changed.
fn annotate_and_emit_refusals(parsed: &mut serde_json::Value, strict: bool) -> bool {
    // Invocation-level errors produced by other CLI guards have no per-op
    // result array. Preserve their shape while still honoring a known token.
    if let Some(reason) = parsed
        .get("error")
        .and_then(|error| error.get("reason"))
        .and_then(serde_json::Value::as_str)
        .and_then(RefusalReason::from_token)
    {
        emit_refusal(reason);
        return false;
    }

    let failed = parsed["summary"]["failed"].as_u64().unwrap_or(0);
    let aborted = parsed["summary"]["aborted"].as_u64().unwrap_or(0);
    let strict_refusal = strict && (failed > 0 || aborted > 0);
    // Save manifests preserve compact failure metadata instead of the full
    // result payload. Treat that projection exactly like canonical results.
    let entries = parsed.as_object_mut().and_then(|object| {
        if object
            .get("results")
            .is_some_and(serde_json::Value::is_array)
        {
            object
                .get_mut("results")
                .and_then(serde_json::Value::as_array_mut)
        } else {
            object
                .get_mut("failures")
                .and_then(serde_json::Value::as_array_mut)
        }
    });

    let mut changed = false;
    let mut emitted = 0usize;
    if let Some(entries) = entries {
        for entry in entries {
            if entry["ok"].as_bool() == Some(true) {
                continue;
            }

            let specific = entry["reason"].as_str().and_then(RefusalReason::from_token);
            let reason = specific.or(strict_refusal.then_some(RefusalReason::StrictOpFailure));
            if let Some(reason) = reason {
                if specific.is_none() {
                    if let Some(object) = entry.as_object_mut() {
                        object.insert("reason".to_string(), serde_json::json!(reason.as_str()));
                        changed = true;
                    }
                }
                emit_refusal(reason);
                emitted += 1;
            }
        }
    }

    // A legacy aggregate can report failures without retaining per-op rows.
    // Keep strict mode machine-classifiable without inventing missing rows.
    if strict_refusal && emitted == 0 {
        emit_refusal(RefusalReason::StrictOpFailure);
    }
    changed
}

/// Prepare the exact string printed by inline exec. Existing JSON stays
/// byte-for-byte unchanged unless strict-mode annotation added a reason.
fn prepare_exec_output(raw: &str, strict: bool) -> String {
    let Ok(mut parsed) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.to_owned();
    };
    if annotate_and_emit_refusals(&mut parsed, strict) {
        serde_json::to_string(&parsed).expect("serde_json::Value is serializable")
    } else {
        raw.to_owned()
    }
}

enum ExecMode {
    Inline(String),
    OpsFile(PathBuf),
}

fn preflight_inline_ops(ops: &str) -> Result<()> {
    if let Err(error) = khive_request::parse_request(ops) {
        // Keep the CLI's established rendered prose byte-for-byte unchanged;
        // the invocation-level error object carries the structured reason.
        let error = rmcp::ErrorData::invalid_params(error.to_string(), None);
        return Err(report_unscoped_refusal(
            RefusalReason::ParseError,
            error.to_string(),
        ));
    }
    Ok(())
}

/// Validate the selected carrier before any actor expectation or dispatch gate.
/// The operation list remains authoritative only after this succeeds.
fn preflight_exec_mode(mode: &ExecMode) -> Result<()> {
    match mode {
        ExecMode::Inline(ops) => preflight_inline_ops(ops),
        ExecMode::OpsFile(path) => match validate_ops_file(path) {
            Ok(_) => Ok(()),
            Err(error) => {
                if let Some(refusal) = error.downcast_ref::<ExecRefusal>() {
                    Err(report_unscoped_refusal(
                        refusal.reason,
                        refusal.message.as_str(),
                    ))
                } else {
                    Err(error)
                }
            }
        },
    }
}

/// Report an invocation-level refusal against the real operation set whenever
/// that set can be parsed without dispatch. In particular, `--expect-actor`
/// mismatches happen before execution but a valid ops-file is still safe to
/// read and parse for its tool names; reporting one synthetic operation would
/// make `summary.total` and per-op correlation false.
fn report_mode_refusal(
    mode: &ExecMode,
    reason: RefusalReason,
    error: impl std::fmt::Display,
) -> anyhow::Error {
    let message = error.to_string();
    let parsed = match mode {
        ExecMode::Inline(raw) => khive_request::parse_request(raw).ok().map(|request| {
            let chain = request.mode == khive_request::ExecutionMode::Chain;
            let tools = request.ops.into_iter().map(|op| op.tool).collect();
            (tools, chain)
        }),
        ExecMode::OpsFile(path) => validate_ops_file(path).ok().and_then(|mut validated| {
            validated_tool_names(&mut validated.snapshot)
                .ok()
                .map(|tools| (tools, false))
        }),
    };
    match parsed {
        Some((tools, chain)) if !tools.is_empty() => {
            report_tools_refusal(tools, chain, reason, message)
        }
        _ => report_unscoped_refusal(reason, message),
    }
}

/// Issue #1586: disclose the resolved database target(s) once, before any
/// dispatch, so a no-override invocation's silent default
/// (`$HOME/.khive/khive.db` — the production database for most installs) is
/// visible. Emitted after the caller has loaded the `[[backends]]` topology,
/// so the line names the config-declared backend targets when those — not
/// `cfg.db_path` — are what receive writes. Stderr rather than a tracing
/// record because kkernel's default log level is `warn` (an INFO record would
/// never surface) and stdout is reserved for JSON results. Best-effort write:
/// the disclosure is nonessential, so a closed or failing stderr must not
/// become an exec failure (`eprintln!` panics on a failed stderr write).
/// Disclosure only: no prompt, no refusal.
fn disclose_resolved_database(cfg: &RuntimeConfig, khive_cfg: &KhiveConfig) {
    use std::io::Write;
    let line =
        khive_mcp::serve::resolved_database_disclosure(cfg.db_path.as_deref(), &khive_cfg.backends);
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[derive(Default)]
struct ExecDbContext {
    raw: Option<String>,
    anchor: Option<PathBuf>,
    config: Option<PathBuf>,
}

fn load_exec_config(db_context: &ExecDbContext) -> Result<(KhiveConfig, Option<PathBuf>)> {
    let db_path_for_config = config_discovery_db_anchor(db_context.raw.as_deref());
    let loaded = KhiveConfig::load_with_home_fallback_and_source(
        db_context.config.as_deref(),
        db_path_for_config.as_deref(),
    )
    .map_err(|e| anyhow::anyhow!("config error: {e}"))?;
    Ok(match loaded {
        Some((config, source)) => (config, Some(source)),
        None => (KhiveConfig::default(), None),
    })
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
    mut cfg: RuntimeConfig,
    presentation: Option<String>,
    output_format: Option<String>,
    save_file: Option<String>,
    mut db_context: ExecDbContext,
    strict: bool,
    #[cfg(unix)] forward_fn: ForwardFnPtr,
) -> Result<()> {
    // Keep this local preflight even though `run_exec` already performs it:
    // tests and internal callers exercise this seam directly, and no caller may
    // let an identity refusal mask malformed DSL.
    preflight_inline_ops(&ops)?;

    // ── strict-actor gate (before any forwarding) ─────────────────────────────
    // Must run BEFORE the daemon fast-path so that a comm-capable anonymous daemon
    // already running cannot be used to bypass KHIVE_REQUIRE_ATTRIBUTED_ACTOR=1.
    // The daemon receives requests over a socket and dispatches comm verbs — the
    // same tenant-isolation risk as in-process dispatch.  Checking only in the
    // in-process fallback (as was the case before this fix) allowed a strict-mode
    // client to silently forward through a pre-existing anonymous daemon and exit 0.
    if let Err(error) = enforce_strict_actor_mode(cfg.actor_id.as_deref(), &cfg.packs) {
        return Err(report_invocation_refusal(
            Some(&ops),
            RefusalReason::AnonymousActor,
            error,
        ));
    }

    // Load the resolved `KhiveConfig` ONCE, up front, so both the daemon
    // forward-frame `config_id` below and the in-process fallback's backend
    // topology (further below) resolve from the identical TOML file the
    // daemon's own boot path loads (`serve.rs`'s `build_server`:
    // `KhiveConfig::load_with_home_fallback(args.config.as_deref(),
    // config_discovery_db_anchor(args.db.as_deref()).as_deref())` —
    // `kkernel exec` threads its `--config` / `KHIVE_CONFIG` selection through
    // this reload exactly like there. The second argument is the raw
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
    let (khive_cfg, config_source) = load_exec_config(&db_context)?;

    // #1226: apply the same --db/[[backends]] conflict guard the in-process
    // fallback below applies, BEFORE the daemon fast-path — otherwise a warm
    // daemon answers this request without the override ever being checked at
    // all, while the identical override on `--ops-file` (always in-process)
    // correctly rejects it. A matching concrete override is redundant, so its
    // fingerprint and captured construction anchor are normalized to the same
    // values used when no override is supplied.
    let force_memory = if khive_cfg.backends.is_empty() {
        false
    } else {
        let force_memory = normalize_redundant_db_override_with_source(
            &mut cfg,
            db_context.raw.as_deref(),
            &khive_cfg.backends,
            config_source.as_deref(),
        )?;
        if matches!(db_context.raw.as_deref(), Some(path) if path != ":memory:") {
            db_context.anchor = cfg.db_path.clone();
        }
        force_memory
    };

    if !force_memory {
        validate_declared_backend_access_modes(&khive_cfg.backends)?;
    }

    disclose_resolved_database(&cfg, &khive_cfg);

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
            process_ref: khive_runtime::process_ref_from_env(),
            visible_namespaces: cfg
                .visible_namespaces
                .iter()
                .map(|ns| ns.as_str().to_string())
                .collect(),
            // Fold the SAME backends topology the daemon folds (`Some(&khive_cfg)`)
            // instead of `None` — see the `khive_cfg` load above. A force-memory
            // override also supplies the effective writable mode explicitly:
            // the declaration can say `main.read_only = true`, but the runtime
            // the child opens is writable memory and fingerprints that captured
            // mode after construction.
            config_id: if force_memory {
                compute_config_id_with_storage_mode(&cfg, Some(&khive_cfg), false)
            } else {
                compute_config_id(&cfg, Some(&khive_cfg))
            },
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
        // Which override a daemon this call may need to SPAWN must be
        // constructed with (the spawn seam forwards whatever it receives):
        // - `:memory:` always: the child must stay ephemeral like the client.
        // - A concrete override in the SINGLE-backend case (no `[[backends]]`
        //   declared above): the fresh daemon has no config-declared database
        //   path, so without the override it would bind `$HOME/.khive/khive.db`
        //   and its `config_id` would never match this override-anchored frame.
        // - A concrete override here in the MULTI-backend case is, by
        //   construction, the redundant-main one proven and normalized above —
        //   withhold it, because the frame's fingerprint is already normalized
        //   to the no-override anchor and the spawned daemon's config-declared
        //   `main` path IS the override's target; forwarding it would desync
        //   the child's `config_id` from the normalized frame.
        let spawn_db = match db_context.raw.as_deref() {
            Some(":memory:") => Some(":memory:"),
            Some(concrete) if khive_cfg.backends.is_empty() => Some(concrete),
            _ => None,
        };
        // Which config file a daemon this call may need to SPAWN must be
        // constructed with:
        // - An explicit `--config`/`KHIVE_CONFIG` selection always: it is the
        //   operator's choice and the frame already folds its topology.
        // - Otherwise, in exactly the redundant-multi-backend case withheld
        //   above: the config that declared the backend topology was
        //   DISCOVERED (retained in `config_source` — e.g. via the db-dir
        //   tier-3 anchor of `KhiveConfig::load_with_home_fallback_and_source`),
        //   and the withheld override was the child's only other clue about
        //   which database to bind. Without forwarding the resolved path as
        //   the child's explicit `--config`, the spawned daemon re-discovers
        //   from its own cwd/HOME, fails to reach a config anchored only
        //   beside the database, binds `$HOME/.khive/khive.db`, and squats
        //   the socket with a `config_id` that never matches this frame.
        //   Forwarding the retained path makes the child fold the identical
        //   topology, so its fingerprint matches.
        // - Otherwise nothing: the empty-backends child gets its database
        //   directly via the forwarded concrete override above.
        let spawn_config = match (&db_context.config, db_context.raw.as_deref()) {
            (Some(explicit), _) => Some(explicit.clone()),
            (None, Some(raw)) if raw != ":memory:" && !khive_cfg.backends.is_empty() => {
                config_source.clone()
            }
            _ => None,
        };
        if let Some(res) = forward_fn(&frame, spawn_config, spawn_db).await {
            let output = res.map_err(|e| anyhow::anyhow!("{}", e.message))?;
            let output = prepare_exec_output(&output, strict);
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
        .dispatch_request_local_for_exec(params, strict)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let output = prepare_exec_output(&output, strict);
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
    output_format: Option<String>,
    save_file: Option<String>,
    dry_run: bool,
    db_context: ExecDbContext,
    atomic: bool,
    atomic_max_ops: Option<usize>,
    strict: bool,
) -> Result<()> {
    // Validate the whole file and spool a stable bounded snapshot before any
    // runtime construction or writes. Non-atomic dispatch retains only one
    // request chunk plus ordered result envelopes in memory.
    let mut validated = match validate_ops_file(&path) {
        Ok(validated) => validated,
        Err(error) => {
            if let Some(refusal) = error.downcast_ref::<ExecRefusal>() {
                return Err(report_unscoped_refusal(
                    refusal.reason,
                    refusal.message.as_str(),
                ));
            }
            return Err(error);
        }
    };

    if validated.total == 0 {
        anyhow::bail!("ops-file is empty (no non-blank lines): {}", path.display());
    }

    if dry_run {
        let summary = serde_json::json!({
            "dry_run": true,
            "total": validated.total,
            "per_verb": validated.per_verb,
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
    if let Err(error) = enforce_strict_actor_mode(cfg.actor_id.as_deref(), &cfg.packs) {
        let tools = validated_tool_names(&mut validated.snapshot)?;
        return Err(report_tools_refusal(
            tools,
            false,
            RefusalReason::AnonymousActor,
            error.to_string(),
        ));
    }
    let (khive_cfg, config_source) = load_exec_config(&db_context)?;

    if !khive_cfg.backends.is_empty() {
        // Preserve the selected config path on a refusal, but leave accepted
        // cases to their downstream owner: the non-atomic shared builder logs
        // and normalizes them once, while `--atomic` rejects multi-backend
        // topology before opening storage.
        reject_conflicting_db_override_with_source(
            db_context.raw.as_deref(),
            &khive_cfg.backends,
            config_source.as_deref(),
        )?;
    }

    disclose_resolved_database(&cfg, &khive_cfg);

    if atomic {
        let max_ops = atomic_max_ops.unwrap_or(khive_types::pack::ATOMIC_MAX_OPS_DEFAULT);
        let ops =
            parse_atomic_validated_snapshot(&mut validated.snapshot, validated.total, max_ops)?;
        // Preflight a deterministic save target before the atomic unit can
        // commit. An execution error drops the unfinished sibling temp file
        // and leaves any prior complete destination untouched.
        let save_sink = save_file
            .as_deref()
            .map(|path| khive_mcp::save_sink::JsonlSaveSink::new(Path::new(path), false))
            .transpose()?;
        let mut envelope =
            match crate::atomic_apply::execute_atomic_ops_file(ops, cfg, &khive_cfg, max_ops).await
            {
                Ok(envelope) => envelope,
                Err(error) => {
                    if let Some(failure) =
                        error.downcast_ref::<crate::atomic_apply::AtomicExecFailure>()
                    {
                        let mut envelope = failure.envelope();
                        annotate_and_emit_refusals(&mut envelope, strict);
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&envelope)
                                .expect("serialize atomic refusal envelope")
                        );
                    }
                    return Err(error);
                }
            };
        annotate_and_emit_refusals(&mut envelope, strict);
        let output = if let Some(save_sink) = save_sink {
            let manifest = save_sink.write_envelope(&envelope)?;
            serde_json::to_string(&manifest).expect("serialize atomic save manifest")
        } else {
            serde_json::to_string_pretty(&envelope).expect("serialize atomic envelope")
        };
        println!("{output}");
        return Ok(());
    }

    let server = build_local_fallback_server(
        cfg,
        &khive_cfg,
        db_context.raw.as_deref(),
        db_context.anchor.as_deref(),
    )?;

    validated
        .snapshot
        .rewind()
        .context("rewind validated ops-file snapshot for dispatch")?;
    apply_ops_file_reader(
        &server,
        std::io::BufReader::new(validated.snapshot),
        validated.total,
        presentation,
        output_format,
        save_file,
        strict,
    )
    .await
    .map(|_| ())
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
        let failures = collect_op_failures(&parsed, 500, OpsFileReportMode::LegacyNoSave);
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
        let failures = collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave);
        assert_eq!(
            failures[0]["error"],
            serde_json::json!({"kind": "invalid_input", "message": "content rejected"}),
            "structured KhiveError payloads pass through as JSON, not a placeholder"
        );
    }

    #[test]
    fn collect_op_failures_preserves_stable_refusal_reason() {
        let parsed = serde_json::json!({
            "results": [
                {
                    "ok": false,
                    "tool": "not_loaded",
                    "error": "unknown verb",
                    "reason": "verb-refused"
                },
            ],
            "summary": {"total": 1, "succeeded": 0, "failed": 1}
        });
        let failures = collect_op_failures(&parsed, 9, OpsFileReportMode::BoundedSave);
        assert_eq!(failures[0]["reason"], "verb-refused");
        assert_eq!(failures[0]["op_index"], 9);
        let legacy = collect_op_failures(&parsed, 9, OpsFileReportMode::LegacyNoSave);
        assert!(
            legacy[0].get("reason").is_none(),
            "legacy no-save summary must retain its pre-reason wire shape"
        );
    }

    #[test]
    fn collect_op_failures_empty_on_all_ok_or_missing_results() {
        let all_ok = serde_json::json!({
            "results": [{"ok": true, "tool": "stats", "result": {}}],
            "summary": {"total": 1, "succeeded": 1, "failed": 0}
        });
        assert!(collect_op_failures(&all_ok, 0, OpsFileReportMode::LegacyNoSave).is_empty());
        assert!(
            collect_op_failures(&serde_json::json!({}), 0, OpsFileReportMode::LegacyNoSave)
                .is_empty()
        );
    }

    #[test]
    fn no_save_reporting_matches_exact_legacy_golden() {
        let parsed = serde_json::json!({
            "results": [
                {"ok": true, "tool": "create", "result": {}},
                {"ok": false, "tool": "search", "error": "boom"},
            ],
        });
        let failures = collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave);
        assert_eq!(failures.len(), 1);
        assert!(failures[0].get("aborted").is_none());
        let summary = ops_file_summary(OpsFileReportMode::LegacyNoSave, 2, 1, 1, 0, failures, 0);

        assert_eq!(
            ops_file_progress_line(OpsFileReportMode::LegacyNoSave, 2, 2, 1, 1, 0),
            "applied 2/2 (ok=1, failed=1)"
        );
        assert_eq!(
            serde_json::to_string_pretty(&summary).unwrap(),
            concat!(
                "{\n",
                "  \"failed\": 1,\n",
                "  \"failures\": [\n",
                "    {\n",
                "      \"error\": \"boom\",\n",
                "      \"op_index\": 1,\n",
                "      \"tool\": \"search\"\n",
                "    }\n",
                "  ],\n",
                "  \"succeeded\": 1,\n",
                "  \"total\": 2\n",
                "}"
            )
        );
        assert!(summary.get("aborted").is_none());
        assert!(summary.get("failure_details_omitted").is_none());
    }

    #[test]
    fn no_save_reporting_retains_more_than_one_thousand_failures() {
        let parsed = serde_json::json!({
            "results": (0..=MAX_OPS_FILE_FAILURE_DETAILS)
                .map(|index| serde_json::json!({
                    "ok": false,
                    "tool": "create",
                    "error": format!("failure-{index}"),
                }))
                .collect::<Vec<_>>(),
        });
        let mut retained = Vec::new();
        let mut omitted = 0;
        for failure in collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave) {
            assert!(retain_failure_detail(
                OpsFileReportMode::LegacyNoSave,
                failure,
                &mut retained,
                &mut omitted,
            ));
        }
        assert_eq!(retained.len(), MAX_OPS_FILE_FAILURE_DETAILS + 1);
        assert_eq!(omitted, 0);

        let summary = ops_file_summary(
            OpsFileReportMode::LegacyNoSave,
            retained.len(),
            0,
            retained.len(),
            0,
            retained,
            omitted,
        );
        assert_eq!(
            summary["failures"].as_array().unwrap().len(),
            MAX_OPS_FILE_FAILURE_DETAILS + 1
        );
        assert!(summary.get("failure_details_omitted").is_none());
    }

    #[test]
    fn no_save_reporting_retains_error_larger_than_four_kib() {
        let large_error = "x".repeat(MAX_OPS_FILE_FAILURE_ERROR_BYTES + 1);
        let parsed = serde_json::json!({
            "results": [{"ok": false, "tool": "create", "error": large_error}],
        });
        let failures = collect_op_failures(&parsed, 0, OpsFileReportMode::LegacyNoSave);
        assert_eq!(
            failures[0]["error"].as_str().unwrap().len(),
            MAX_OPS_FILE_FAILURE_ERROR_BYTES + 1
        );
        assert_eq!(failures[0]["error"], large_error);
    }

    #[test]
    fn save_reporting_bounds_failure_count_and_error_detail() {
        let large_error = "x".repeat(MAX_OPS_FILE_FAILURE_ERROR_BYTES + 1);
        let parsed = serde_json::json!({
            "results": (0..=MAX_OPS_FILE_FAILURE_DETAILS)
                .map(|index| serde_json::json!({
                    "ok": false,
                    "tool": "create",
                    "aborted": index % 2 == 0,
                    "error": if index == 0 {
                        serde_json::Value::String(large_error.clone())
                    } else {
                        serde_json::Value::String(format!("failure-{index}"))
                    },
                }))
                .collect::<Vec<_>>(),
        });
        let mut retained = Vec::new();
        let mut omitted = 0;
        for failure in collect_op_failures(&parsed, 0, OpsFileReportMode::BoundedSave) {
            retain_failure_detail(
                OpsFileReportMode::BoundedSave,
                failure,
                &mut retained,
                &mut omitted,
            );
        }
        assert_eq!(retained.len(), MAX_OPS_FILE_FAILURE_DETAILS);
        assert_eq!(omitted, 1);
        assert_eq!(retained[0]["aborted"], true);
        assert_eq!(
            retained[0]["error"],
            format!(
                "error detail omitted: exceeds {MAX_OPS_FILE_FAILURE_ERROR_BYTES}-byte ops-file diagnostic limit"
            )
        );

        let summary = ops_file_summary(
            OpsFileReportMode::BoundedSave,
            MAX_OPS_FILE_FAILURE_DETAILS + 1,
            0,
            MAX_OPS_FILE_FAILURE_DETAILS + 1,
            0,
            retained,
            omitted,
        );
        assert_eq!(summary["failure_details_omitted"], 1);
        assert_eq!(
            summary["failures"].as_array().unwrap().len(),
            MAX_OPS_FILE_FAILURE_DETAILS
        );
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
    #[serial]
    fn config_flag_and_env_bind_with_flag_precedence() {
        let previous = std::env::var_os("KHIVE_CONFIG");
        std::env::set_var("KHIVE_CONFIG", "/tmp/kkernel-exec-env-config.toml");

        let from_env = ExecArgs::parse_from(["exec", "stats()"]);
        assert_eq!(
            from_env.config.as_deref(),
            Some(std::path::Path::new("/tmp/kkernel-exec-env-config.toml"))
        );

        let from_flag = ExecArgs::parse_from([
            "exec",
            "stats()",
            "--config",
            "/tmp/kkernel-exec-flag-config.toml",
        ]);
        assert_eq!(
            from_flag.config.as_deref(),
            Some(std::path::Path::new("/tmp/kkernel-exec-flag-config.toml"))
        );

        match previous {
            Some(value) => std::env::set_var("KHIVE_CONFIG", value),
            None => std::env::remove_var("KHIVE_CONFIG"),
        }
    }

    #[test]
    fn explicit_config_flag_parses_for_exec() {
        let args = ExecArgs::parse_from([
            "exec",
            "stats()",
            "--config",
            "/tmp/kkernel-exec-config.toml",
        ]);
        assert_eq!(
            args.config.as_deref(),
            Some(std::path::Path::new("/tmp/kkernel-exec-config.toml"))
        );
    }

    #[test]
    fn actor_and_expect_actor_flags_parse_together() {
        let args = ExecArgs::parse_from([
            "exec",
            "stats()",
            "--actor",
            "lambda:worker",
            "--expect-actor",
            "lambda:worker",
        ]);
        assert_eq!(args.actor.as_deref(), Some("lambda:worker"));
        assert_eq!(args.expect_actor.as_deref(), Some("lambda:worker"));
    }

    #[test]
    #[serial]
    fn khive_actor_env_does_not_bind_to_explicit_actor_arg() {
        let previous = std::env::var("KHIVE_ACTOR").ok();
        std::env::set_var("KHIVE_ACTOR", "lambda:env");
        let args = ExecArgs::parse_from(["exec", "stats()"]);
        match previous {
            Some(value) => std::env::set_var("KHIVE_ACTOR", value),
            None => std::env::remove_var("KHIVE_ACTOR"),
        }
        assert_eq!(args.actor, None, "the env fallback must not become tier 1");
    }

    #[test]
    fn actor_flags_conflict_with_pending_events_mode() {
        assert!(ExecArgs::try_parse_from(
            ["exec", "--pending-events", "--actor", "lambda:worker",]
        )
        .is_err());
        assert!(ExecArgs::try_parse_from([
            "exec",
            "--pending-events",
            "--expect-actor",
            "lambda:worker",
        ])
        .is_err());
    }

    #[test]
    fn explicit_actor_overrides_fallback_without_changing_namespace() {
        let mut cfg = RuntimeConfig {
            default_namespace: Namespace::parse("project:data").unwrap(),
            actor_id: Some("lambda:fallback".to_string()),
            visible_namespaces: vec![Namespace::parse("lambda:fallback").unwrap()],
            ..RuntimeConfig::default()
        };
        apply_actor_pin_and_expectation(&mut cfg, Some("lambda:cli"), Some("lambda:cli")).unwrap();
        assert_eq!(cfg.actor_id.as_deref(), Some("lambda:cli"));
        assert_eq!(cfg.default_namespace.as_str(), "project:data");
        assert_eq!(
            cfg.visible_namespaces,
            vec![Namespace::parse("lambda:cli").unwrap()],
            "pinning must drop the displaced actor's folded read visibility and add the pinned one"
        );
    }

    #[test]
    fn explicit_local_actor_authoritatively_clears_fallback() {
        let mut cfg = RuntimeConfig {
            actor_id: Some("lambda:fallback".to_string()),
            visible_namespaces: vec![Namespace::parse("lambda:fallback").unwrap()],
            ..RuntimeConfig::default()
        };
        apply_actor_pin_and_expectation(&mut cfg, Some("local"), Some("local")).unwrap();
        assert_eq!(cfg.actor_id, None);
        assert!(
            cfg.visible_namespaces.is_empty(),
            "pinning to local must drop the displaced fallback actor's read visibility \
             without adding a replacement: {:?}",
            cfg.visible_namespaces
        );
    }

    #[test]
    fn explicit_actor_pin_retains_unrelated_configured_visibility() {
        let mut cfg = RuntimeConfig {
            actor_id: Some("lambda:fallback".to_string()),
            visible_namespaces: vec![
                Namespace::parse("lambda:fallback").unwrap(),
                Namespace::parse("project:shared").unwrap(),
            ],
            ..RuntimeConfig::default()
        };
        apply_actor_pin_and_expectation(&mut cfg, Some("lambda:cli"), None).unwrap();
        assert_eq!(
            cfg.visible_namespaces,
            vec![
                Namespace::parse("project:shared").unwrap(),
                Namespace::parse("lambda:cli").unwrap(),
            ],
            "an explicitly configured extra visibility entry unrelated to the displaced \
             actor must survive the pin: {:?}",
            cfg.visible_namespaces
        );
    }

    #[test]
    fn expect_actor_alone_validates_resolved_identity() {
        let mut cfg = RuntimeConfig {
            actor_id: Some("lambda:project".to_string()),
            ..RuntimeConfig::default()
        };
        apply_actor_pin_and_expectation(&mut cfg, None, Some("lambda:project")).unwrap();
        let err = apply_actor_pin_and_expectation(&mut cfg, None, Some("lambda:other"))
            .expect_err("a mismatched expectation must fail before dispatch");
        assert!(err.to_string().contains("--expect-actor mismatch"));
        assert!(err.to_string().contains("lambda:project"));
    }

    #[test]
    fn actor_inputs_are_namespace_validated() {
        let mut cfg = RuntimeConfig::default();
        assert!(apply_actor_pin_and_expectation(&mut cfg, Some("bad actor"), None).is_err());
        assert!(apply_actor_pin_and_expectation(&mut cfg, None, Some("bad actor")).is_err());
    }

    #[tokio::test]
    #[serial]
    async fn authorized_explicit_actor_is_used_for_write_attribution() {
        let (previous_home, _home_dir) = isolate_home_for_test();
        let mut cfg = RuntimeConfig {
            db_path: None,
            actor_id: Some("lambda:fallback".to_string()),
            gate: std::sync::Arc::new(khive_runtime::AllowAllGate),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string(), "comm".to_string()],
            ..RuntimeConfig::default()
        };
        apply_actor_pin_and_expectation(&mut cfg, Some("lambda:pinned"), None).unwrap();
        let server = build_local_fallback_server(cfg, &KhiveConfig::default(), None, None).unwrap();
        let raw = server
            .dispatch_request_local(RequestParams {
                ops: r#"comm.send(to="local", content="actor pin attribution")"#.to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .unwrap();
        restore_home(previous_home);

        let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            response["results"][0]["ok"], true,
            "comm.send dispatch failed: {raw}"
        );
        assert_eq!(response["results"][0]["result"]["from"], "lambda:pinned");
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
            // from the ambient environment (#1276) — callers of this helper
            // dispatch `kg` and `gtd.assign` verbs, so pin both rather than
            // letting a wider ambient pack set a developer's shell exports.
            packs: vec!["kg".to_string(), "gtd".to_string()],
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
    fn isolated_server_ignores_ambient_khive_packs_naming_unavailable_pack() {
        const CHILD_MARKER: &str = "KKERNEL_KHIVE_PACKS_TEST_CHILD";
        const TEST_NAME: &str =
            "exec::tests::isolated_server_ignores_ambient_khive_packs_naming_unavailable_pack";

        if std::env::var_os(CHILD_MARKER).is_none() {
            let status = std::process::Command::new(
                std::env::current_exe().expect("current test executable"),
            )
            .arg(TEST_NAME)
            .arg("--exact")
            .env("KHIVE_PACKS", "kg,gtd")
            .env(CHILD_MARKER, "1")
            .status()
            .expect("spawn isolated KHIVE_PACKS test process");
            assert!(status.success(), "isolated child test failed: {status}");
            return;
        }

        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        // Before the fix, this panicked inside `KhiveMcpServer::new` — the
        // helper inherited the ambient list above instead of pinning its own.
        let _server = isolated_server(&db_path);
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

        // Exec-shaped inputs with no explicit config in this scenario and
        // `namespace_explicit: true` (the choice made in `run_exec` above).
        // Pin the pack list explicitly rather than inheriting `KHIVE_PACKS`
        // from the ambient environment (same rationale as `isolated_server`
        // above, #1276): `RuntimeConfig::default()` reads `KHIVE_PACKS` fresh
        // on every call, so leaving this `None` makes the assertion below
        // depend on two independent env reads observing the same ambient
        // value — a real flake source when a concurrently-running test
        // mutates process env between them (#1356).
        let pinned_packs = Some(vec!["kg".to_string()]);

        let exec_cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&db_str),
            config: None,
            namespace: ns.clone(),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: false,
            packs: pinned_packs.clone(),
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
            packs: pinned_packs,
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

    /// Regression guard: an explicit `--actor` pin must rebuild the
    /// actor-derived portion of `visible_namespaces`, not just `actor_id`.
    ///
    /// Builds the config exactly the way `run_exec` does — through
    /// `resolve_runtime_config`, from a project `[actor] id = "lambda:fallback"`
    /// with no explicit extra visibility — so the displaced actor is folded
    /// into `visible_namespaces` (ADR-007 Rev 4 Rule 3b) before the pin is
    /// ever applied. A non-local pin must give default reads `local ∪
    /// lambda:pinned`; a `local` pin must leave only `local`. Neither case may
    /// retain `lambda:fallback`.
    #[test]
    #[serial]
    fn actor_pin_rebuilds_visible_namespaces_dropping_displaced_fallback() {
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
id = "lambda:fallback"
"#,
        )
        .expect("write config.toml");

        let db_path = khive_dir.join("actor-pin-visibility-test.db");
        let db_str = db_path.to_str().expect("utf8 path").to_string();
        let pinned_packs = Some(vec!["kg".to_string()]);

        let resolve = |db: &str| {
            resolve_runtime_config(RuntimeConfigInputs {
                db: Some(db),
                config: None,
                namespace: Namespace::parse("local").expect("ns"),
                namespace_explicit: true,
                actor_explicit: false,
                no_embed: true,
                packs: pinned_packs.clone(),
                brain_profile: None,
            })
            .expect("resolve exec-shaped config")
        };

        // Sanity: the fallback actor really is folded into the default read
        // visible-set before any pin is applied — otherwise this test would
        // pass vacuously.
        let baseline = resolve(&db_str);
        assert_eq!(baseline.actor_id.as_deref(), Some("lambda:fallback"));
        assert!(baseline
            .visible_namespaces
            .contains(&Namespace::parse("lambda:fallback").expect("ns")));

        // A non-local pin must replace the fallback's read visibility with the
        // pinned actor's — never both, never neither.
        let mut pinned_cfg = resolve(&db_str);
        apply_actor_pin_and_expectation(&mut pinned_cfg, Some("lambda:pinned"), None).unwrap();
        assert_eq!(pinned_cfg.actor_id.as_deref(), Some("lambda:pinned"));
        assert!(
            pinned_cfg
                .visible_namespaces
                .contains(&Namespace::parse("lambda:pinned").expect("ns")),
            "pinned actor must be added to the default read scope: {:?}",
            pinned_cfg.visible_namespaces
        );
        assert!(
            !pinned_cfg
                .visible_namespaces
                .contains(&Namespace::parse("lambda:fallback").expect("ns")),
            "the displaced fallback actor must not remain visible under the pinned \
             identity: {:?}",
            pinned_cfg.visible_namespaces
        );

        // A `local` pin must authoritatively clear the fallback's visibility
        // without adding a replacement.
        let mut local_cfg = resolve(&db_str);
        apply_actor_pin_and_expectation(&mut local_cfg, Some("local"), None).unwrap();
        assert_eq!(local_cfg.actor_id, None);
        assert!(
            local_cfg.visible_namespaces.is_empty(),
            "pinning to local must leave only local visible, retaining neither the \
             fallback actor nor adding a new one: {:?}",
            local_cfg.visible_namespaces
        );
    }

    /// Settles the `namespace_explicit` design question by constructing both
    /// arms and comparing `compute_config_id` directly, per the decision
    /// criterion: does either arm break config_id parity with the daemon?
    ///
    /// No `[actor] id` is present (an explicit EMPTY config file makes
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

        // A real, EMPTY config file: the explicit tier fails loud on a
        // missing file (ADR-035), so the hermeticity trick must be a real
        // file with no `[actor]` block.
        let empty_config_dir = tempfile::tempdir().expect("empty config tempdir");
        let missing_config = empty_config_dir.path().join("config.toml");
        std::fs::write(&missing_config, "").expect("write empty config");
        let ns = Namespace::parse("lambda:custom-ns").expect("ns");
        // Pin packs so the `compute_config_id` comparison below never depends
        // on two independent `KHIVE_PACKS` env reads agreeing (#1356).
        let pinned_packs = Some(vec!["kg".to_string()]);

        let with_explicit_true = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: ns.clone(),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: false,
            packs: pinned_packs.clone(),
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
            packs: pinned_packs,
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

        // An explicit EMPTY config file keeps this fully deterministic
        // regardless of host state (same rationale as the sibling test
        // above; the explicit tier fails loud on a MISSING file — ADR-035 —
        // so the trick must be a real file).
        let empty_config_dir = tempfile::tempdir().expect("empty config tempdir");
        let missing_config = empty_config_dir.path().join("multi-backend-config.toml");
        std::fs::write(&missing_config, "").expect("write empty config");
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

        // Pin packs so the config_id comparisons below never depend on two
        // independent `KHIVE_PACKS` env reads agreeing (#1356).
        let pinned_packs = Some(vec!["kg".to_string()]);

        // exec-shaped inputs (namespace_explicit: true — the choice `run_exec` makes).
        let exec_cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&missing_config),
            namespace: ns.clone(),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: false,
            packs: pinned_packs.clone(),
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
            packs: pinned_packs,
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
                    "comm".to_string(),
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
            packs: vec!["kg".to_string(), "comm".to_string()],
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

        let send = server
            .dispatch_request_local(RequestParams {
                ops: r#"comm.send(to="actor-routing-test", content="routed-via-secondary", self_send=true)"#
                    .to_string(),
                presentation: None,
                presentation_per_op: None,
                save_to: None,
                format: None,
                format_per_op: None,
                request_id: None,
            })
            .await
            .expect("comm.send must dispatch");
        let send_resp: serde_json::Value = serde_json::from_str(&send).expect("valid JSON");
        assert_eq!(
            send_resp["results"][0]["ok"].as_bool(),
            Some(true),
            "comm.send must succeed through the multi-backend fallback server: {send_resp}"
        );

        // Re-open EACH backend file independently (fresh KhiveMcpServer, no
        // shared state) and list `message` notes directly against it.
        async fn count_messages(db_path: &std::path::Path) -> usize {
            let cfg = RuntimeConfig {
                db_path: Some(db_path.to_path_buf()),
                embedding_model: None,
                additional_embedding_models: vec![],
                packs: vec!["kg".to_string(), "comm".to_string()],
                ..RuntimeConfig::default()
            };
            let rt = KhiveRuntime::new(cfg).expect("runtime on backend file");
            let probe = KhiveMcpServer::new(rt).expect("server on backend file");
            let raw = probe
                .dispatch_request_local(RequestParams {
                    ops: r#"list(kind="message")"#.to_string(),
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

        let main_count = count_messages(&main_path).await;
        let secondary_count = count_messages(&secondary_path).await;

        assert_eq!(
            main_count, 0,
            "comm pack must NOT write into the `main` backend file when pinned to \
             `secondary` (D1-R2: a silent single-backend fallback would have written \
             it here instead)"
        );
        assert_eq!(
            secondary_count, 2,
            "comm pack write must land in its declared `secondary` backend file — \
             `comm.send` dual-writes an outbound + inbound note copy per message \
             (khive-pack-comm's message.rs), both via the SAME pack runtime, so a \
             single self-send yields 2 `message` notes in whichever backend `comm` \
             is pinned to"
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
        let ops = parse_ops_file(f.path()).unwrap();
        assert_eq!(ops.len(), 2);
    }

    #[test]
    fn parse_ops_file_reports_line_number_on_malformed() {
        use std::io::Write as _;
        let mut f = NamedTempFile::new().unwrap();
        f.write_all(b"{\"tool\":\"stats\",\"args\":{}}\n").unwrap();
        f.write_all(b"not-json\n").unwrap(); // line 2 is bad
        let err = parse_ops_file(f.path()).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ExecRefusal>().map(|error| error.reason),
            Some(RefusalReason::ParseError)
        );
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
        let err = parse_ops_file(f.path()).unwrap_err();
        assert_eq!(
            err.downcast_ref::<ExecRefusal>().map(|error| error.reason),
            Some(RefusalReason::ParseError)
        );
        let msg = format!("{err:#}");
        assert!(msg.contains("line 1"), "should report line number: {msg}");
    }

    #[test]
    fn atomic_op_limit_is_checked_before_snapshot_materialization() {
        struct PanicOnSnapshotAccess;

        impl std::io::Read for PanicOnSnapshotAccess {
            fn read(&mut self, _buf: &mut [u8]) -> std::io::Result<usize> {
                panic!("over-limit atomic snapshot must not be read or materialized")
            }
        }

        impl std::io::Seek for PanicOnSnapshotAccess {
            fn seek(&mut self, _pos: std::io::SeekFrom) -> std::io::Result<u64> {
                panic!("over-limit atomic snapshot must not be rewound or materialized")
            }
        }

        let error = parse_atomic_validated_snapshot(&mut PanicOnSnapshotAccess, 2, 1)
            .expect_err("the validated op count exceeds the configured atomic ceiling");
        assert!(
            error
                .to_string()
                .contains("op count 2 exceeds the configured maximum 1"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn ops_file_physical_line_and_total_caps_are_fail_closed() {
        let mut within = std::io::Cursor::new(b"1234567\n".to_vec());
        assert_eq!(
            read_bounded_ops_line_with_limit(&mut within, 1, 8)
                .unwrap()
                .unwrap(),
            "1234567"
        );
        let mut over = std::io::Cursor::new(b"12345678\n".to_vec());
        let error = read_bounded_ops_line_with_limit(&mut over, 7, 8).unwrap_err();
        assert!(error.to_string().contains("line 7"));

        let oversized = NamedTempFile::new().unwrap();
        oversized.as_file().set_len(MAX_OPS_FILE_BYTES + 1).unwrap();
        let error = validate_ops_file(oversized.path()).unwrap_err();
        assert!(error.to_string().contains("total limit"));
    }

    #[test]
    fn large_ops_file_payload_is_read_from_path_not_argv() {
        let mut file = NamedTempFile::new().unwrap();
        let payload = "x".repeat(1024 * 1024);
        serde_json::to_writer(
            &mut file,
            &serde_json::json!({"tool":"stats","args":{"payload":payload}}),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();

        let path = file.path().to_str().unwrap();
        let args = ExecArgs::try_parse_from(["exec", "--ops-file", path]).unwrap();
        assert!(args.ops.is_none());
        assert_eq!(args.ops_file.as_deref(), Some(file.path()));
        assert_eq!(validate_ops_file(file.path()).unwrap().total, 1);
    }

    #[test]
    fn chunk_byte_boundary_is_exact() {
        assert!(!should_defer_chunk_entry(
            0,
            0,
            OPS_FILE_CHUNK_MAX_BYTES + 1
        ));
        assert!(!should_defer_chunk_entry(
            1,
            OPS_FILE_CHUNK_MAX_BYTES - 1,
            1
        ));
        assert!(should_defer_chunk_entry(1, OPS_FILE_CHUNK_MAX_BYTES, 1));
    }

    #[test]
    fn ordered_chunk_contract_rejects_tool_or_summary_drift() {
        let ops = vec![
            OpsFileEntry {
                tool: "first".to_string(),
                args: serde_json::json!({}),
            },
            OpsFileEntry {
                tool: "second".to_string(),
                args: serde_json::json!({}),
            },
            OpsFileEntry {
                tool: "third".to_string(),
                args: serde_json::json!({}),
            },
        ];
        let valid = serde_json::json!({
            "results": [
                {"ok":true,"tool":"first","result":{}},
                {"ok":false,"tool":"second","error":"no"},
                {"ok":false,"tool":"third","aborted":true,"error":"not attempted"}
            ],
            "summary":{"total":3,"succeeded":1,"failed":1,"aborted":1},
            "status":"partial"
        });
        assert_eq!(
            validate_ordered_chunk_envelope(&ops, &valid, 1).unwrap(),
            (1, 1, 1)
        );

        let mut wrong_tool = valid.clone();
        wrong_tool["results"][1]["tool"] = serde_json::json!("third");
        assert!(validate_ordered_chunk_envelope(&ops, &wrong_tool, 1).is_err());

        let mut lying_summary = valid;
        lying_summary["summary"]["succeeded"] = serde_json::json!(2);
        lying_summary["summary"]["failed"] = serde_json::json!(0);
        assert!(validate_ordered_chunk_envelope(&ops, &lying_summary, 1).is_err());

        let mut missing_result = serde_json::json!({
            "results": [
                {"ok":true,"tool":"first"},
                {"ok":false,"tool":"second","error":"no"},
                {"ok":false,"tool":"third","aborted":true,"error":"not attempted"}
            ],
            "summary":{"total":3,"succeeded":1,"failed":1,"aborted":1},
            "status":"partial"
        });
        assert!(validate_ordered_chunk_envelope(&ops, &missing_result, 1).is_err());
        missing_result["results"][0]["result"] = serde_json::Value::Null;
        missing_result["results"][1]
            .as_object_mut()
            .unwrap()
            .remove("error");
        assert!(validate_ordered_chunk_envelope(&ops, &missing_result, 1).is_err());

        let mut contradictory = serde_json::json!({
            "results": [
                {"ok":true,"tool":"first","result":null,"error":null},
                {"ok":false,"tool":"second","error":"no","result":null},
                {"ok":false,"tool":"third","aborted":true,"error":"not attempted"}
            ],
            "summary":{"total":3,"succeeded":1,"failed":1,"aborted":1},
            "status":"partial"
        });
        assert!(validate_ordered_chunk_envelope(&ops, &contradictory, 1).is_err());
        contradictory["results"][0]
            .as_object_mut()
            .unwrap()
            .remove("error");
        assert!(validate_ordered_chunk_envelope(&ops, &contradictory, 1).is_err());
    }

    fn status_contract_fixture(status: &str) -> (Vec<OpsFileEntry>, serde_json::Value) {
        (
            vec![
                OpsFileEntry {
                    tool: "first".to_string(),
                    args: serde_json::json!({}),
                },
                OpsFileEntry {
                    tool: "second".to_string(),
                    args: serde_json::json!({}),
                },
            ],
            serde_json::json!({
                "results": [
                    {"ok":true,"tool":"first","result":{}},
                    {"ok":false,"tool":"second","error":"no"}
                ],
                "summary":{"total":2,"succeeded":1,"failed":1,"aborted":0},
                "status":status
            }),
        )
    }

    #[test]
    fn ordered_chunk_truthful_status_passes() {
        let (ops, envelope) = status_contract_fixture("partial");
        assert_eq!(
            validate_ordered_chunk_envelope(&ops, &envelope, 1).unwrap(),
            (1, 1, 0)
        );
    }

    #[test]
    fn ordered_chunk_contradicting_status_is_rejected() {
        let (ops, envelope) = status_contract_fixture("success");
        let error = validate_ordered_chunk_envelope(&ops, &envelope, 1).unwrap_err();
        assert!(error.to_string().contains("status"));
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

        let ops = parse_ops_file(f.path()).unwrap();
        assert_eq!(ops.len(), 3);
        let summary = apply_ops_file(&server, ops, None, None, None, false)
            .await
            .unwrap();
        assert_eq!(summary["total"], 3);
        assert_eq!(summary["succeeded"], 3);
        assert_eq!(summary["failed"], 0);
        assert!(summary.get("aborted").is_none());
        assert!(summary.get("failure_details_omitted").is_none());
        assert!(summary.get("results").is_none());

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

    #[tokio::test]
    async fn multi_chunk_save_retains_order_rows_checksum_and_json_override() {
        use sha2::Digest as _;

        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let server = isolated_server(&db_path)
            .with_default_output_format(khive_runtime::OutputFormat::Table);
        let ops: Vec<OpsFileEntry> = (0..=OPS_FILE_CHUNK_SIZE)
            .map(|index| OpsFileEntry {
                tool: "create".to_string(),
                args: serde_json::json!({
                    "kind": "concept",
                    "name": format!("ordered-{index:03}"),
                }),
            })
            .collect();
        let output_dir = tempfile::tempdir().unwrap();
        let save_path = output_dir.path().join("ordered.jsonl");

        let manifest = apply_ops_file(
            &server,
            ops,
            Some("verbose".to_string()),
            Some("json".to_string()),
            Some(save_path.to_string_lossy().into_owned()),
            true,
        )
        .await
        .unwrap();

        let manifest_keys: std::collections::BTreeSet<_> = manifest
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        assert_eq!(
            manifest_keys,
            std::collections::BTreeSet::from([
                "checksum",
                "path",
                "per_column_null_counts",
                "rows",
                "schema_fingerprint",
                "summary",
            ]),
            "the successful manifest shape must remain unchanged"
        );
        assert_eq!(manifest["rows"], OPS_FILE_CHUNK_SIZE + 1);
        assert_eq!(manifest["summary"]["total"], OPS_FILE_CHUNK_SIZE + 1);
        assert_eq!(manifest["summary"]["succeeded"], OPS_FILE_CHUNK_SIZE + 1);
        assert_eq!(manifest["summary"]["failed"], 0);
        assert_eq!(manifest["summary"]["aborted"], 0);

        let bytes = std::fs::read(&save_path).unwrap();
        let checksum = format!("{:x}", sha2::Sha256::digest(&bytes));
        assert_eq!(manifest["checksum"], checksum);
        let rows: Vec<serde_json::Value> = bytes
            .split(|byte| *byte == b'\n')
            .filter(|line| !line.is_empty())
            .map(|line| serde_json::from_slice(line).unwrap())
            .collect();
        assert_eq!(rows.len(), OPS_FILE_CHUNK_SIZE + 1);
        for (index, row) in rows.iter().enumerate() {
            assert_eq!(row["tool"], "create");
            assert_eq!(row["ok"], true);
            assert_eq!(row["result"]["name"], format!("ordered-{index:03}"));
        }
    }

    #[tokio::test]
    async fn malformed_later_chunk_emits_aborted_manifest_for_prior_commits() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let server = isolated_server(&db_path);
        let ops: Vec<OpsFileEntry> = (0..=OPS_FILE_CHUNK_SIZE)
            .map(|index| OpsFileEntry {
                tool: "create".to_string(),
                args: serde_json::json!({
                    "kind": "concept",
                    "name": format!("abort-manifest-{index:03}"),
                }),
            })
            .collect();
        let output_dir = tempfile::tempdir().unwrap();
        let save_path = output_dir.path().join("must-not-publish.jsonl");

        let error = apply_ops_file_with_response_transform(
            &server,
            ops,
            Some("verbose".to_string()),
            Some("json".to_string()),
            Some(save_path.to_string_lossy().into_owned()),
            true,
            |chunk_number, raw| {
                if chunk_number == 2 {
                    "{malformed-response".to_string()
                } else {
                    raw
                }
            },
        )
        .await
        .unwrap_err();

        let aborted = error
            .downcast_ref::<AbortedOpsFileError>()
            .expect("post-dispatch failure must carry its emitted manifest");
        assert_eq!(aborted.manifest["status"], "aborted");
        assert_eq!(aborted.manifest["committed_chunks"], serde_json::json!([1]));
        assert_eq!(aborted.manifest["dispatched_chunk"], 2);
        assert_eq!(aborted.manifest["file_published"], false);
        assert_eq!(
            aborted.manifest["summary"]["succeeded"],
            OPS_FILE_CHUNK_SIZE
        );
        assert_eq!(aborted.manifest["summary"]["total"], OPS_FILE_CHUNK_SIZE);
        assert_eq!(aborted.manifest["summary"]["aborted"], 0);
        assert_eq!(aborted.manifest["unconfirmed_ops"], 1);
        assert!(
            !save_path.exists(),
            "an aborted run must not publish partial JSONL"
        );

        // `committed_chunks: [1]` is a claim about DURABLE STATE, and the manifest
        // asserting it is assembled locally. Every assertion above would still pass
        // if chunk 1's writes had been rolled back or never reached storage, because
        // the bookkeeping would simply agree with itself. Read it back through the
        // same server so the reconciliation record is checked against the database
        // it describes. The requested limit stays under the entity list cap, so the
        // handler returns a bare array rather than a clamp-wrapped object.
        let params = RequestParams {
            ops: r#"list(kind="concept", limit=200)"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        };
        let raw = server.dispatch_request_local(params).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
        let rows = response["results"][0]["result"]
            .as_array()
            .unwrap_or_else(|| {
                panic!(
                    "read-back must return a bare array under the entity list cap; got {}",
                    response["results"][0]["result"]
                )
            });
        // A row that carries no string name is an unreadable instrument, not an
        // absent entity, so it panics here rather than being dropped silently.
        let mut observed: Vec<String> = rows
            .iter()
            .map(|row| {
                row["name"]
                    .as_str()
                    .unwrap_or_else(|| panic!("read-back row carries no string name: {row}"))
                    .to_owned()
            })
            .collect();
        // An empty or unparsed read-back is an instrument failure, not a pass.
        assert!(
            !observed.is_empty(),
            "read-back yielded no rows; result was {}",
            response["results"][0]["result"]
        );
        observed.sort_unstable();

        // Enumerate the two legal outcomes instead of bounding a count. Entity
        // names carry no uniqueness constraint and the upsert is keyed by UUID,
        // so any count of distinct names is a proxy: a duplicate row satisfies
        // it while the property it stands for is broken. Comparing the whole
        // sorted list pins which rows are present, and how many of each.
        let committed: Vec<String> = (0..OPS_FILE_CHUNK_SIZE)
            .map(|index| format!("abort-manifest-{index:03}"))
            .collect();
        // Chunk 2 was dispatched without a verified response, so its single op
        // may or may not have landed. The manifest reports it as unconfirmed
        // rather than committed precisely because both outcomes are legal here.
        let mut with_unconfirmed = committed.clone();
        with_unconfirmed.push(format!("abort-manifest-{OPS_FILE_CHUNK_SIZE:03}"));

        assert!(
            observed == committed || observed == with_unconfirmed,
            "manifest reports chunk 1 committed and chunk 2 unconfirmed, so the database must \
             hold exactly the {OPS_FILE_CHUNK_SIZE} confirmed rows, optionally plus the one \
             unconfirmed row; found {} rows: {observed:?}",
            observed.len()
        );
    }

    #[tokio::test]
    async fn invalid_save_directory_is_rejected_before_any_op_side_effect() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let server = isolated_server(&db_path);
        let output_dir = tempfile::tempdir().unwrap();
        let ops = vec![OpsFileEntry {
            tool: "create".to_string(),
            args: serde_json::json!({"kind":"concept","name":"must-not-exist"}),
        }];

        let error = apply_ops_file(
            &server,
            ops,
            Some("verbose".to_string()),
            Some("json".to_string()),
            Some(output_dir.path().to_string_lossy().into_owned()),
            true,
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("absent or an existing regular file"));

        let params = RequestParams {
            ops: r#"list(kind="concept")"#.to_string(),
            presentation: None,
            presentation_per_op: None,
            save_to: None,
            format: Some("json".to_string()),
            format_per_op: None,
            request_id: None,
        };
        let raw = server.dispatch_request_local(params).await.unwrap();
        let response: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(response["results"][0]["result"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn save_manifest_preserves_partial_summary_and_strict_writes_rows() {
        fn partial_ops(success_name: &str) -> Vec<OpsFileEntry> {
            vec![
                OpsFileEntry {
                    tool: "create".to_string(),
                    args: serde_json::json!({"kind":"concept","name":success_name}),
                },
                OpsFileEntry {
                    tool: "search".to_string(),
                    args: serde_json::json!({"kind":"not_a_real_kind","query":"x"}),
                },
            ]
        }

        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let server = isolated_server(&db_path);
        let output_dir = tempfile::tempdir().unwrap();
        let save_path = output_dir.path().join("partial.jsonl");
        let manifest = apply_ops_file(
            &server,
            partial_ops("partial-ok"),
            Some("verbose".to_string()),
            Some("json".to_string()),
            Some(save_path.to_string_lossy().into_owned()),
            false,
        )
        .await
        .unwrap();
        assert_eq!(manifest["rows"], 2);
        assert_eq!(manifest["summary"]["succeeded"], 1);
        assert_eq!(manifest["summary"]["failed"], 1);
        assert_eq!(manifest["summary"]["aborted"], 0);

        let strict_path = output_dir.path().join("strict-partial.jsonl");
        let error = apply_ops_file(
            &server,
            partial_ops("strict-partial-ok"),
            Some("verbose".to_string()),
            Some("json".to_string()),
            Some(strict_path.to_string_lossy().into_owned()),
            true,
        )
        .await
        .unwrap_err();
        assert!(error.to_string().contains("--strict"));
        assert_eq!(
            std::fs::read_to_string(strict_path)
                .unwrap()
                .lines()
                .count(),
            2
        );
    }

    // ── #1220: --strict exit-code signal for partially-failed batches ─────────

    #[test]
    fn prepare_exec_output_preserves_specific_reasons_and_fills_strict_failures() {
        let raw = serde_json::json!({
            "results": [
                {"ok": true, "tool": "stats", "result": {}},
                {"ok": false, "tool": "get", "error": "missing id"},
                {
                    "ok": false,
                    "tool": "not_loaded",
                    "error": "unknown verb",
                    "reason": "verb-refused"
                },
                {"ok": false, "tool": "update", "aborted": true},
            ],
            "summary": {"total": 4, "succeeded": 1, "failed": 2, "aborted": 1},
            "status": "partial",
        })
        .to_string();

        let parsed: serde_json::Value =
            serde_json::from_str(&prepare_exec_output(&raw, true)).unwrap();
        assert_eq!(parsed["results"][1]["reason"], "strict-op-failure");
        assert_eq!(parsed["results"][2]["reason"], "verb-refused");
        assert_eq!(parsed["results"][3]["reason"], "strict-op-failure");
        assert!(parsed["results"][0].get("reason").is_none());
    }

    #[test]
    fn enforce_strict_batch_result_ok_when_strict_off_and_partially_failed() {
        let raw = serde_json::json!({
            "results": [],
            "summary": {"total": 2, "succeeded": 1, "failed": 1, "aborted": 0},
        })
        .to_string();
        assert!(enforce_strict_batch_result(&raw, false).is_ok());
    }

    // ── #1339: fully-failed batches exit non-zero even without --strict ──────

    #[test]
    fn enforce_strict_batch_result_errs_when_strict_off_and_every_op_failed() {
        let raw = serde_json::json!({
            "results": [],
            "summary": {"total": 1, "succeeded": 0, "failed": 1, "aborted": 0},
        })
        .to_string();
        let err = enforce_strict_batch_result(&raw, false).unwrap_err();
        assert!(format!("{err}").contains("every op failed"));
    }

    #[test]
    fn enforce_strict_batch_result_errs_when_strict_off_and_chain_fully_aborted() {
        let raw = serde_json::json!({
            "results": [],
            "summary": {"total": 3, "succeeded": 0, "failed": 1, "aborted": 2},
        })
        .to_string();
        assert!(enforce_strict_batch_result(&raw, false).is_err());
    }

    #[test]
    fn enforce_strict_batch_result_ok_on_empty_batch_summary() {
        let raw = serde_json::json!({
            "results": [],
            "summary": {"total": 0, "succeeded": 0, "failed": 0, "aborted": 0},
        })
        .to_string();
        assert!(enforce_strict_batch_result(&raw, false).is_ok());
        assert!(enforce_strict_batch_result(&raw, true).is_ok());
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

        let ops = parse_ops_file(f.path()).unwrap();
        assert_eq!(ops.len(), 2);

        let err = apply_ops_file(&server, ops, None, None, None, true)
            .await
            .expect_err("strict mode must surface the per-op failure as a process error");
        assert!(format!("{err}").contains("1 op(s) failed"));
    }

    #[tokio::test]
    async fn apply_ops_file_errs_without_strict_when_every_op_fails() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let server = isolated_server(&db_path);

        let mut f = NamedTempFile::new().unwrap();
        use std::io::Write as _;
        f.write_all(
            b"{\"tool\":\"search\",\"args\":{\"kind\":\"not_a_real_kind\",\"query\":\"x\"}}\n",
        )
        .unwrap();

        let ops = parse_ops_file(f.path()).unwrap();
        let err = apply_ops_file(&server, ops, None, None, None, false)
            .await
            .expect_err("a fully-failed ops-file must exit non-zero even without --strict");
        assert!(format!("{err}").contains("every op failed"));
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
            None,
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

    #[derive(Debug)]
    struct DenyPinnedActorGate {
        observed: std::sync::Arc<std::sync::Mutex<Vec<String>>>,
    }

    impl khive_runtime::Gate for DenyPinnedActorGate {
        fn check(
            &self,
            req: &khive_runtime::GateRequest,
        ) -> std::result::Result<khive_runtime::GateDecision, khive_runtime::GateError> {
            self.observed.lock().unwrap().push(req.actor.id.clone());
            if req.actor.id == "lambda:pinned" {
                Ok(khive_runtime::GateDecision::deny(
                    "test actor is not granted",
                ))
            } else {
                Ok(khive_runtime::GateDecision::allow())
            }
        }
    }

    #[tokio::test]
    #[serial]
    async fn unauthorized_explicit_actor_is_not_retried_as_fallback() {
        let previous_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();
        std::env::set_var("KHIVE_NO_DAEMON", "1");
        let (previous_home, _home_dir) = isolate_home_for_test();
        let db_file = NamedTempFile::new().expect("temp db");
        let observed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let mut cfg = RuntimeConfig {
            db_path: Some(db_file.path().to_path_buf()),
            actor_id: Some("lambda:fallback".to_string()),
            gate: std::sync::Arc::new(DenyPinnedActorGate {
                observed: observed.clone(),
            }),
            packs: vec!["kg".to_string()],
            ..RuntimeConfig::default()
        };
        apply_actor_pin_and_expectation(&mut cfg, Some("lambda:pinned"), Some("lambda:pinned"))
            .unwrap();

        let result = run_exec_inline(
            r#"create(kind="concept", name="MustNotExist")"#.to_string(),
            cfg,
            None,
            None,
            None,
            ExecDbContext::default(),
            false,
        )
        .await;

        match previous_no_daemon {
            Some(value) => std::env::set_var("KHIVE_NO_DAEMON", value),
            None => std::env::remove_var("KHIVE_NO_DAEMON"),
        }
        restore_home(previous_home);

        assert!(result.is_err(), "the gate refusal must be terminal");
        let observed = observed.lock().unwrap();
        assert!(
            !observed.is_empty(),
            "the configured gate must be consulted"
        );
        assert!(
            observed.iter().all(|actor| actor == "lambda:pinned"),
            "no gate check may retry as the displaced fallback actor: {observed:?}"
        );
    }

    // ── strict-actor mode: daemon bypass regression ───────────────────────────

    /// Security regression: strict-actor gate must fire before daemon forward.
    /// See `crates/kkernel/docs/design.md#execrs-regression-test-notes`.
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
        let (prev_home, _home_dir) = isolate_home_for_test();

        std::env::set_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR", "1");
        std::env::set_var("KHIVE_NO_DAEMON", "1"); // force in-process to avoid daemon dep

        let cfg = RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string(), "comm".to_string()],
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
    /// run_exec_inline must NOT reject even with comm + no actor (OSS default path).
    #[tokio::test]
    #[serial]
    async fn strict_mode_off_exec_inline_passes_with_comm_no_actor() {
        let prev_strict = std::env::var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR").ok();
        let prev_no_daemon = std::env::var("KHIVE_NO_DAEMON").ok();
        let (prev_home, _home_dir) = isolate_home_for_test();

        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR"); // default OFF
        std::env::set_var("KHIVE_NO_DAEMON", "1");

        let cfg = RuntimeConfig {
            db_path: None,
            packs: vec!["kg".to_string(), "comm".to_string()],
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
    // ISOMORPHISM PROOF:
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
    fn spy_forward_records_call<'a>(
        _frame: &'a DaemonRequestFrame,
        _config: Option<PathBuf>,
        _db: Option<&'a str>,
    ) -> super::ForwardFuture<'a> {
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
            ExecDbContext::default(),
            false,
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
        let (prev_home, _home_dir) = isolate_home_for_test();
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
            ExecDbContext::default(),
            false,
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
        restore_home(prev_home);

        assert!(
            result.is_ok(),
            "gate must pass when actor is configured; got: {result:?}"
        );
        assert!(
            spy_was_called,
            "spy forward_fn must be called when gate passes (KHIVE_NO_DAEMON=1 causes in-process fallback)"
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
        static SPY_CAPTURED_CONFIG_PATH: std::cell::RefCell<Option<PathBuf>> =
            const { std::cell::RefCell::new(None) };
        static SPY_CAPTURED_DB: std::cell::RefCell<Option<String>> =
            const { std::cell::RefCell::new(None) };
    }

    #[cfg(unix)]
    fn spy_capture_config_id<'a>(
        frame: &'a DaemonRequestFrame,
        config: Option<PathBuf>,
        db: Option<&'a str>,
    ) -> super::ForwardFuture<'a> {
        SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = Some(frame.config_id.clone()));
        SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = config);
        SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = db.map(str::to_string));
        Box::pin(async { None })
    }

    #[cfg(unix)]
    fn spy_capture_config_and_succeed<'a>(
        frame: &'a DaemonRequestFrame,
        config: Option<PathBuf>,
        db: Option<&'a str>,
    ) -> super::ForwardFuture<'a> {
        SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = Some(frame.config_id.clone()));
        SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = config);
        SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = db.map(str::to_string));
        Box::pin(async {
            Some(Ok(
                r#"{"results":[{"ok":true,"tool":"stats","result":{}}],"summary":{"total":1,"succeeded":1,"failed":0}}"#
                    .to_string(),
            ))
        })
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn explicit_config_reaches_daemon_spawn_seam() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);

        let dir = tempfile::tempdir().expect("config tempdir");
        let config_path = dir.path().join("selected.toml");
        std::fs::write(&config_path, "[runtime]\npacks = [\"kg\"]\n")
            .expect("write explicit config");

        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: None,
            config: Some(&config_path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
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
            ExecDbContext {
                raw: None,
                anchor: None,
                config: Some(config_path.clone()),
            },
            false,
            spy_capture_config_and_succeed,
        )
        .await;

        assert!(
            result.is_ok(),
            "forwarded dispatch must succeed: {result:?}"
        );
        assert_eq!(
            SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
            Some(config_path),
            "the daemon spawn seam must receive the same explicit config path used to resolve the exec frame"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn memory_db_override_reaches_daemon_spawn_seam() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        std::env::remove_var("KHIVE_DB");
        SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);

        let dir = tempfile::tempdir().expect("config tempdir");
        let config_path = dir.path().join("selected.toml");
        std::fs::write(&config_path, "[runtime]\npacks = [\"kg\"]\n")
            .expect("write explicit config");

        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&config_path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
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
            ExecDbContext {
                raw: Some(":memory:".to_string()),
                anchor: None,
                config: Some(config_path.clone()),
            },
            false,
            spy_capture_config_and_succeed,
        )
        .await;

        assert!(
            result.is_ok(),
            "forwarded dispatch must succeed: {result:?}"
        );
        assert_eq!(
            SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
            Some(":memory:".to_string()),
            "the daemon spawn seam must receive the raw --db override so a spawned daemon \
             can be constructed with the same ephemeral in-memory storage"
        );
    }

    /// A declared read-only SQLite `main` backend becomes a writable memory
    /// backend when `--db :memory:` forces the whole topology ephemeral. The
    /// pre-open exec frame must fingerprint that effective runtime mode, not
    /// the superseded declaration, or the freshly spawned daemon rejects its
    /// very first request as a config mismatch.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn force_memory_exec_frame_matches_opened_read_only_topology_runtime() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        std::env::remove_var("KHIVE_DB");
        let (prev_home, _home_dir) = isolate_home_for_test();
        SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

        let fixture = tempfile::tempdir().expect("force-memory config tempdir");
        let config_path = fixture.path().join("read-only-topology.toml");
        let declared_main = fixture.path().join("declared-main.db");
        let declared_archive = fixture.path().join("declared-archive.db");
        std::fs::write(
            &config_path,
            format!(
                r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"
read_only = true

[[backends]]
name = "archive"
kind = "sqlite"
path = "{}"
"#,
                declared_main.display(),
                declared_archive.display(),
            ),
        )
        .expect("write read-only topology config");

        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(":memory:"),
            config: Some(&config_path),
            namespace: Namespace::local(),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve force-memory exec config");
        assert_eq!(
            cfg.db_path, None,
            "the force-memory anchor must be in-memory"
        );

        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            cfg.clone(),
            None,
            None,
            None,
            ExecDbContext {
                raw: Some(":memory:".to_string()),
                anchor: None,
                config: Some(config_path.clone()),
            },
            false,
            spy_capture_config_and_succeed,
        )
        .await;
        assert!(result.is_ok(), "force-memory dispatch failed: {result:?}");

        let frame_config_id = SPY_CAPTURED_CONFIG_ID
            .with(|captured| captured.borrow_mut().take())
            .expect("spy must capture the forwarded config id");
        let khive_cfg = KhiveConfig::load_with_home_fallback(Some(&config_path), None)
            .expect("load force-memory topology")
            .expect("explicit config must exist");
        let opened =
            khive_mcp::serve::build_registry_for_multi_backend(cfg, &khive_cfg, Some(":memory:"))
                .expect("force-memory runtime must build");
        restore_home(prev_home);

        assert!(
            !opened.default_runtime.is_read_only(),
            "force-memory replaces the declared read-only SQLite main with writable memory"
        );
        assert_eq!(
            frame_config_id, opened.config_id,
            "the pre-open exec frame and opened force-memory runtime must have identical config ids"
        );
        assert!(
            !declared_main.exists() && !declared_archive.exists(),
            "force-memory parity setup must not materialize either declared SQLite path"
        );
    }

    #[cfg(unix)]
    fn write_writable_multi_backend_config(
        config_path: &Path,
        main_path: &Path,
        secondary_path: &Path,
    ) {
        std::fs::write(
            config_path,
            format!(
                r#"
[[backends]]
name = "main"
kind = "sqlite"
path = "{}"

[[backends]]
name = "archive"
kind = "sqlite"
path = "{}"
"#,
                main_path.display(),
                secondary_path.display(),
            ),
        )
        .unwrap();
    }

    #[cfg(unix)]
    fn chmod_read_only(path: &Path) {
        use std::os::unix::fs::PermissionsExt;

        let mut permissions = std::fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o444);
        std::fs::set_permissions(path, permissions).unwrap();
    }

    #[cfg(unix)]
    fn runtime_config_for_explicit_multi_backend(
        config_path: &Path,
        db: Option<&str>,
    ) -> RuntimeConfig {
        resolve_runtime_config(RuntimeConfigInputs {
            db,
            config: Some(config_path),
            namespace: Namespace::local(),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .unwrap()
    }

    /// A client must not reuse a daemon that retained a write-capable handle
    /// after the declared main file was chmod'd into snapshot mode. The
    /// filesystem-mode refusal belongs before the forwarding seam.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn multi_backend_main_chmod_refuses_before_daemon_forward() {
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

        let fixture = tempfile::tempdir().unwrap();
        let config_path = fixture.path().join("khive.toml");
        let main_path = fixture.path().join("main.db");
        let archive_path = fixture.path().join("archive.db");
        std::fs::write(&main_path, b"main snapshot fixture").unwrap();
        std::fs::write(&archive_path, b"archive fixture").unwrap();
        chmod_read_only(&main_path);
        write_writable_multi_backend_config(&config_path, &main_path, &archive_path);

        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            runtime_config_for_explicit_multi_backend(&config_path, None),
            None,
            None,
            None,
            ExecDbContext {
                raw: None,
                anchor: None,
                config: Some(config_path),
            },
            false,
            spy_capture_config_and_succeed,
        )
        .await;

        let error = result.expect_err("an undeclared main snapshot mode must fail closed");
        assert!(error.to_string().contains("read_only = true"), "{error}");
        assert!(
            SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow().is_none()),
            "the retained writable daemon must never receive the frame"
        );
    }

    /// The topology fingerprint includes secondary modes too; apply the same
    /// pre-forward refusal to every declared SQLite backend, not only `main`.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn multi_backend_secondary_chmod_refuses_before_daemon_forward() {
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

        let fixture = tempfile::tempdir().unwrap();
        let config_path = fixture.path().join("khive.toml");
        let main_path = fixture.path().join("main.db");
        let archive_path = fixture.path().join("archive.db");
        std::fs::write(&main_path, b"main fixture").unwrap();
        std::fs::write(&archive_path, b"archive snapshot fixture").unwrap();
        chmod_read_only(&archive_path);
        write_writable_multi_backend_config(&config_path, &main_path, &archive_path);

        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            runtime_config_for_explicit_multi_backend(&config_path, None),
            None,
            None,
            None,
            ExecDbContext {
                raw: None,
                anchor: None,
                config: Some(config_path),
            },
            false,
            spy_capture_config_and_succeed,
        )
        .await;

        let error = result.expect_err("an undeclared secondary snapshot mode must fail closed");
        assert!(error.to_string().contains("archive"), "{error}");
        assert!(
            SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow().is_none()),
            "the retained writable daemon must never receive the frame"
        );
    }

    /// `--db :memory:` supersedes every declared file. Its pre-open/runtime
    /// parity must therefore skip filesystem-mode checks on those unused paths.
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn force_memory_skips_declared_chmod_preflight_and_forwards() {
        std::env::remove_var("KHIVE_DB");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

        let fixture = tempfile::tempdir().unwrap();
        let config_path = fixture.path().join("khive.toml");
        let main_path = fixture.path().join("main.db");
        let archive_path = fixture.path().join("archive.db");
        std::fs::write(&main_path, b"unused main snapshot fixture").unwrap();
        std::fs::write(&archive_path, b"unused archive snapshot fixture").unwrap();
        chmod_read_only(&main_path);
        chmod_read_only(&archive_path);
        write_writable_multi_backend_config(&config_path, &main_path, &archive_path);

        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            runtime_config_for_explicit_multi_backend(&config_path, Some(":memory:")),
            None,
            None,
            None,
            ExecDbContext {
                raw: Some(":memory:".to_string()),
                anchor: None,
                config: Some(config_path),
            },
            false,
            spy_capture_config_and_succeed,
        )
        .await;

        assert!(
            result.is_ok(),
            "force-memory forwarding must remain valid: {result:?}"
        );
        assert!(
            SPY_CAPTURED_CONFIG_ID.with(|captured| captured.borrow().is_some()),
            "the force-memory frame must reach the forwarding seam"
        );
    }

    /// A CONCRETE override on a single-backend invocation (no `[[backends]]`
    /// declared) must reach the spawn seam: the spawned daemon has no
    /// config-declared database path and would otherwise bind
    /// `$HOME/.khive/khive.db`, never matching the client's override-anchored
    /// frame. (The redundant multi-backend concrete case is withheld — see
    /// `inline_db_override_guard_normalizes_main_config_id_and_rejects_conflict`.)
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn single_backend_concrete_db_override_reaches_daemon_spawn_seam() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        std::env::remove_var("KHIVE_DB");
        SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);

        let dir = tempfile::tempdir().expect("config tempdir");
        // No [[backends]] declared — the single-backend shape.
        let config_path = dir.path().join("selected.toml");
        std::fs::write(&config_path, "[runtime]\npacks = [\"kg\"]\n")
            .expect("write explicit config");

        let override_path = dir.path().join("override.db");
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(override_path.to_str().expect("utf8")),
            config: Some(&config_path),
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
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
            ExecDbContext {
                raw: Some(override_path.display().to_string()),
                anchor: khive_runtime::resolve_db_anchor(override_path.to_str()),
                config: Some(config_path),
            },
            false,
            spy_capture_config_and_succeed,
        )
        .await;

        assert!(
            result.is_ok(),
            "forwarded dispatch must succeed: {result:?}"
        );
        assert_eq!(
            SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
            Some(override_path.display().to_string()),
            "the single-backend concrete override must reach the daemon spawn seam so a \
             spawned daemon binds the operator's file instead of the default database"
        );
    }

    /// The redundant-multi-backend spawn decision (override withheld from the
    /// spawned daemon) has a config-side twin: when no explicit `--config`
    /// was given, the config that declared the backend topology was
    /// DISCOVERED (here via the db-dir tier-3 anchor of
    /// `KhiveConfig::load_with_home_fallback_and_source`), and the withheld
    /// override was the child's only other clue about which database to
    /// bind. The spawn seam must receive that retained resolved path as the
    /// child's explicit `--config`, or the spawned daemon re-discovers from
    /// its own cwd/HOME, cannot reach a config anchored only beside the
    /// database, binds `$HOME/.khive/khive.db`, and squats the socket with a
    /// `config_id` that never matches the normalized frame.
    ///
    /// Control arms: with an explicit config the seam receives the explicit
    /// path (never the discovered one), and in the empty-backends case the
    /// seam receives no config at all (the concrete override supplies the
    /// database directly).
    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn redundant_db_override_forwards_discovered_config_to_spawn_seam() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        std::env::remove_var("KHIVE_DB");
        let (prev_home, _home_dir) = isolate_home_for_test();
        SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);
        SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);

        // A multi-backend config discoverable ONLY via the db-dir tier-3
        // anchor (`project_config_anchor_dir`): it lives in
        // `<main-db-dir>/.khive/config.toml`, not at the process cwd and not
        // under `$HOME/.khive`.
        let backend_dir = tempfile::tempdir().expect("backend tempdir");
        let main_backend_path = backend_dir.path().join("main-backend.db");
        let sessions_backend_path = backend_dir.path().join("sessions-backend.db");
        let anchor_dir = backend_dir.path().join(".khive");
        std::fs::create_dir_all(&anchor_dir).expect("mkdir db-dir anchor");
        let discovered_config_path = anchor_dir.join("config.toml");
        std::fs::write(
            &discovered_config_path,
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
"#,
                main_backend_path.display(),
                sessions_backend_path.display(),
            ),
        )
        .expect("write tier-3 multi-backend config");
        let canonical_config_path =
            std::fs::canonicalize(&discovered_config_path).expect("canonicalize config path");

        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(main_backend_path.to_str().expect("utf8")),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve exec-shaped config");

        // ── the fix case: redundant multi-backend, no explicit config ──
        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            cfg.clone(),
            None,
            None,
            None,
            ExecDbContext {
                raw: Some(main_backend_path.display().to_string()),
                anchor: khive_runtime::resolve_db_anchor(main_backend_path.to_str()),
                config: None,
            },
            false,
            spy_capture_config_and_succeed,
        )
        .await;
        assert!(
            result.is_ok(),
            "redundant-override dispatch must reach daemon forwarding: {result:?}"
        );
        assert_eq!(
            SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
            None,
            "the redundant override stays withheld from the spawn seam"
        );
        assert_eq!(
            SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
            Some(canonical_config_path.clone()),
            "the spawn seam must receive the retained resolved config path as the \
             child's explicit --config when the redundant override is withheld"
        );

        // ── control: explicit config wins, discovered path is not substituted ──
        let explicit_config_path = backend_dir.path().join("explicit.toml");
        std::fs::copy(&discovered_config_path, &explicit_config_path)
            .expect("copy topology as explicit config");
        SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);
        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            cfg.clone(),
            None,
            None,
            None,
            ExecDbContext {
                raw: Some(main_backend_path.display().to_string()),
                anchor: khive_runtime::resolve_db_anchor(main_backend_path.to_str()),
                config: Some(explicit_config_path.clone()),
            },
            false,
            spy_capture_config_and_succeed,
        )
        .await;
        assert!(
            result.is_ok(),
            "explicit-config dispatch must reach daemon forwarding: {result:?}"
        );
        assert_eq!(
            SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
            Some(explicit_config_path),
            "with an explicit config the seam receives the operator's path, never a discovered one"
        );

        // ── control: empty backends get no config, only the concrete override ──
        let single_dir = tempfile::tempdir().expect("single-backend tempdir");
        let override_path = single_dir.path().join("override.db");
        let single_cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(override_path.to_str().expect("utf8")),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve single-backend exec-shaped config");
        SPY_CAPTURED_CONFIG_PATH.with(|c| *c.borrow_mut() = None);
        SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = None);
        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            single_cfg,
            None,
            None,
            None,
            ExecDbContext {
                raw: Some(override_path.display().to_string()),
                anchor: khive_runtime::resolve_db_anchor(override_path.to_str()),
                config: None,
            },
            false,
            spy_capture_config_and_succeed,
        )
        .await;
        assert!(
            result.is_ok(),
            "single-backend dispatch must reach daemon forwarding: {result:?}"
        );
        assert_eq!(
            SPY_CAPTURED_CONFIG_PATH.with(|captured| captured.borrow_mut().take()),
            None,
            "the empty-backends case forwards no config — the concrete override supplies the database"
        );
        assert_eq!(
            SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
            Some(override_path.display().to_string()),
            "the single-backend concrete override still reaches the spawn seam"
        );

        restore_home(prev_home);
    }

    #[cfg(unix)]
    #[tokio::test]
    #[serial]
    async fn explicit_config_is_loaded_for_exec_forward_frame() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        let (prev_home, _home_dir) = isolate_home_for_test();
        SPY_CAPTURED_CONFIG_ID.with(|captured| *captured.borrow_mut() = None);

        let fixture = tempfile::tempdir().expect("config fixture tempdir");
        let config_path = fixture.path().join("code-map.toml");
        let main_backend_path = fixture.path().join("code-map.db");
        let sessions_backend_path = fixture.path().join("sessions.db");
        std::fs::write(
            &config_path,
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
"#,
                main_backend_path.display(),
                sessions_backend_path.display(),
            ),
        )
        .expect("write explicit exec config");

        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: None,
            config: Some(&config_path),
            namespace: Namespace::parse("local").expect("namespace"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve explicit exec config");

        let result = run_exec_inline_with_forward(
            "stats()".to_string(),
            cfg.clone(),
            None,
            None,
            None,
            ExecDbContext {
                raw: None,
                anchor: None,
                config: Some(config_path.clone()),
            },
            false,
            spy_capture_config_id,
        )
        .await;
        assert!(
            result.is_ok(),
            "explicit-config dispatch failed: {result:?}"
        );

        let captured = SPY_CAPTURED_CONFIG_ID
            .with(|value| value.borrow_mut().take())
            .expect("spy must capture the forwarded config id");
        let khive_cfg = KhiveConfig::load_with_home_fallback(Some(&config_path), None)
            .expect("load explicit config")
            .expect("explicit config must exist");
        let expected = compute_config_id(&cfg, Some(&khive_cfg));
        restore_home(prev_home);

        assert_eq!(
            captured, expected,
            "the exec forward frame must fold the explicitly selected backend topology"
        );
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
        // `[[backends]]` and `kkernel exec` relies on default discovery.
        // A divergent explicit `--db` would be rejected as ambiguous once
        // backends are declared; repeating the main path would be accepted but
        // would not model this default-discovery scenario.
        let khive_dir = home_dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
        // Keep the configuration home-shaped while placing the stores in a
        // separate tempdir. Test-harness builds reject every store under
        // `$HOME/.khive`, including isolated fixtures, at the open boundary.
        let backend_dir = tempfile::tempdir().expect("backend tempdir");
        let main_backend_path = backend_dir.path().join("main-backend.db");
        let sessions_backend_path = backend_dir.path().join("sessions-backend.db");
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
    async fn inline_db_override_guard_normalizes_main_config_id_and_rejects_conflict() {
        std::env::remove_var("KHIVE_EMBEDDING_MODEL");
        std::env::remove_var("KHIVE_ADDITIONAL_EMBEDDING_MODELS");
        std::env::remove_var("KHIVE_ACTOR");
        std::env::remove_var("KHIVE_REQUIRE_ATTRIBUTED_ACTOR");
        let (prev_home, home_dir) = isolate_home_for_test();
        SPY_CAPTURED_CONFIG_ID.with(|c| *c.borrow_mut() = None);

        let khive_dir = home_dir.path().join(".khive");
        std::fs::create_dir_all(&khive_dir).expect("mkdir .khive");
        // Keep the configuration home-shaped while placing the stores in a
        // separate tempdir. Test-harness builds reject every store under
        // `$HOME/.khive`, including isolated fixtures, at the open boundary.
        let backend_dir = tempfile::tempdir().expect("backend tempdir");
        let main_backend_path = backend_dir.path().join("main-backend.db");
        let sessions_backend_path = backend_dir.path().join("sessions-backend.db");
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

        let no_override_cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: None,
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            // Pin the pack list rather than inheriting `KHIVE_PACKS` from the
            // ambient environment (#1276) — an ambient list naming packs not
            // compiled into this build would fail resolution before the
            // behavior under test.
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve exec-shaped config without override");
        let no_override_result = run_exec_inline_with_forward(
            "stats()".to_string(),
            no_override_cfg,
            None,
            None,
            None,
            ExecDbContext::default(),
            false,
            spy_capture_config_id,
        )
        .await;
        assert!(
            no_override_result.is_ok(),
            "no-override dispatch must succeed: {no_override_result:?}"
        );
        let no_override_config_id = SPY_CAPTURED_CONFIG_ID
            .with(|captured| captured.borrow_mut().take())
            .expect("no-override frame must be captured");

        let matching_override = main_backend_path.display().to_string();
        // Sentinel: proves a captured None below means "the seam was called
        // with None" (withheld override), not "the spy was never invoked".
        SPY_CAPTURED_DB.with(|c| *c.borrow_mut() = Some("sentinel".to_string()));
        let cfg = resolve_runtime_config(RuntimeConfigInputs {
            db: Some(&matching_override),
            config: None,
            namespace: Namespace::parse("local").expect("ns"),
            namespace_explicit: true,
            actor_explicit: false,
            no_embed: true,
            // Pin the pack list rather than inheriting `KHIVE_PACKS` from the
            // ambient environment (#1276) — an ambient list naming packs not
            // compiled into this build would fail resolution before the
            // behavior under test.
            packs: Some(vec!["kg".to_string()]),
            brain_profile: None,
        })
        .expect("resolve exec-shaped config");

        let matching_result = run_exec_inline_with_forward(
            "stats()".to_string(),
            cfg.clone(),
            None,
            None,
            None,
            ExecDbContext {
                raw: Some(matching_override.clone()),
                anchor: khive_runtime::resolve_db_anchor(Some(&matching_override)),
                config: None,
            },
            false,
            spy_capture_config_id,
        )
        .await;
        assert!(
            matching_result.is_ok(),
            "an override matching the declared main backend must reach daemon forwarding: {matching_result:?}"
        );
        let matching_config_id = SPY_CAPTURED_CONFIG_ID
            .with(|captured| captured.borrow_mut().take())
            .expect("matching-override frame must be captured");
        assert_eq!(
            SPY_CAPTURED_DB.with(|captured| captured.borrow_mut().take()),
            None,
            "the redundant multi-backend concrete override must be WITHHELD from the spawn \
             seam: the frame's fingerprint is normalized to the no-override anchor, and the \
             spawned daemon's config-declared main path IS the override's target"
        );

        let conflicting_override = backend_dir.path().join("override.db");
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
        assert_eq!(
            matching_config_id, no_override_config_id,
            "a matching --db override must emit the same config_id as no override for the same multi-backend config"
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
            // from the ambient environment (#1276). Atomic execution retains
            // the complete discovered validation/lifecycle surface even when
            // the caller configures only the base KG pack.
            packs: vec!["kg".to_string()],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn atomic_kg_only_config_keeps_gtd_hook_and_lifecycle_execution() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let khive_cfg = KhiveConfig::default();

        let (hook_task_id, transition_task_id, complete_task_id) = {
            let server = isolated_server(&db_path);
            let response = dispatch_json(
                &server,
                r#"[gtd.assign(title="HookGuard", status="next"), gtd.assign(title="TransitionGuard", status="inbox"), gtd.assign(title="CompleteGuard", status="active")]"#,
            )
            .await;
            let full_id = |index: usize| {
                response["results"][index]["result"]["full_id"]
                    .as_str()
                    .unwrap_or_else(|| panic!("missing task full_id at index {index}: {response}"))
                    .to_string()
            };
            (full_id(0), full_id(1), full_id(2))
        };

        let hook_error = crate::atomic_apply::execute_atomic_ops_file(
            vec![atomic_op(
                "update",
                serde_json::json!({
                    "id": hook_task_id.as_str(),
                    "properties": {"depends_on": [hook_task_id.as_str()]},
                }),
            )],
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("the GTD task hook must reject a self-dependency");
        assert!(
            format!("{hook_error:#}").contains("cannot depend on itself"),
            "the kg-only atomic registry must enforce the GTD hook: {hook_error:#}"
        );

        let server = isolated_server(&db_path);
        let response = dispatch_json(&server, &format!(r#"get(id="{hook_task_id}")"#)).await;
        assert!(
            response["results"][0]["result"]["properties"]
                .get("depends_on")
                .is_none(),
            "the rejected dependency update must not mutate the task: {response}"
        );

        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            vec![
                atomic_op(
                    "gtd.transition",
                    serde_json::json!({"id": transition_task_id, "status": "next"}),
                ),
                atomic_op(
                    "gtd.complete",
                    serde_json::json!({"id": complete_task_id, "result": "verified"}),
                ),
            ],
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("GTD lifecycle adapters must execute with a kg-only config");
        assert_eq!(envelope["atomic"]["committed"], true, "{envelope}");
        assert_eq!(envelope["results"][0]["result"]["to"], "next");
        assert_eq!(envelope["results"][1]["result"]["to"], "done");
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

    #[tokio::test]
    async fn atomic_rollback_preserves_zero_exit_with_or_without_save_and_strict() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let (x_id, y_id) = {
            let server = isolated_server(&db_path);
            let response = dispatch_json(
                &server,
                r#"[create(kind="concept", name="AtomicExitX"), create(kind="concept", name="AtomicExitY")]"#,
            )
            .await;
            (
                response["results"][0]["result"]["id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
                response["results"][1]["result"]["id"]
                    .as_str()
                    .unwrap()
                    .to_string(),
            )
        };
        let mut file = NamedTempFile::new().unwrap();
        for op in [
            serde_json::json!({"tool":"delete","args":{"id":x_id.clone(),"hard":true}}),
            serde_json::json!({
                "tool":"link",
                "args":{"source_id":y_id,"target_id":x_id,"relation":"extends"}
            }),
        ] {
            serde_json::to_writer(&mut file, &op).unwrap();
            file.write_all(b"\n").unwrap();
        }
        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("khive.toml");
        std::fs::write(&config_path, "").unwrap();
        let db_context = || ExecDbContext {
            raw: Some(db_path.clone()),
            anchor: Some(PathBuf::from(&db_path)),
            config: Some(config_path.clone()),
        };

        run_exec_ops_file(
            file.path().to_path_buf(),
            atomic_cfg(&db_path),
            None,
            None,
            None,
            false,
            db_context(),
            true,
            None,
            true,
        )
        .await
        .expect("atomic rollback remains a successful CLI seam even with --strict");

        let output_dir = tempfile::tempdir().unwrap();
        let save_path = output_dir.path().join("atomic-rollback.jsonl");
        run_exec_ops_file(
            file.path().to_path_buf(),
            atomic_cfg(&db_path),
            None,
            Some("json".to_string()),
            Some(save_path.to_string_lossy().into_owned()),
            false,
            db_context(),
            true,
            None,
            true,
        )
        .await
        .expect("atomic save preserves the existing rollback exit contract");
        assert_eq!(
            std::fs::read_to_string(save_path).unwrap().lines().count(),
            2
        );
    }

    #[tokio::test]
    async fn atomic_invalid_save_directory_is_rejected_before_commit() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let mut file = NamedTempFile::new().unwrap();
        serde_json::to_writer(
            &mut file,
            &serde_json::json!({
                "tool":"create",
                "args":{"kind":"concept","name":"atomic-must-not-exist"}
            }),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();

        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("khive.toml");
        std::fs::write(&config_path, "").unwrap();
        let save_directory = tempfile::tempdir().unwrap();
        let error = run_exec_ops_file(
            file.path().to_path_buf(),
            atomic_cfg(&db_path),
            None,
            Some("json".to_string()),
            Some(save_directory.path().to_string_lossy().into_owned()),
            false,
            ExecDbContext {
                raw: Some(db_path.clone()),
                anchor: Some(PathBuf::from(&db_path)),
                config: Some(config_path),
            },
            true,
            None,
            true,
        )
        .await
        .unwrap_err();
        assert!(error
            .to_string()
            .contains("absent or an existing regular file"));

        let server = isolated_server(&db_path);
        let response = dispatch_json(&server, r#"list(kind="concept")"#).await;
        assert_eq!(response["results"][0]["result"], serde_json::json!([]));
    }

    #[tokio::test]
    async fn atomic_preflighted_save_keeps_prior_file_on_execution_error() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let mut file = NamedTempFile::new().unwrap();
        serde_json::to_writer(
            &mut file,
            &serde_json::json!({
                "tool":"create",
                "args":{"kind":"concept","name":"atomic-rejected"}
            }),
        )
        .unwrap();
        file.write_all(b"\n").unwrap();

        let config_dir = tempfile::tempdir().unwrap();
        let config_path = config_dir.path().join("khive.toml");
        std::fs::write(&config_path, "").unwrap();
        let output_dir = tempfile::tempdir().unwrap();
        let save_path = output_dir.path().join("prior.jsonl");
        std::fs::write(&save_path, b"prior-complete-output\n").unwrap();

        let error = run_exec_ops_file(
            file.path().to_path_buf(),
            atomic_cfg(&db_path),
            None,
            Some("json".to_string()),
            Some(save_path.to_string_lossy().into_owned()),
            false,
            ExecDbContext {
                raw: Some(db_path),
                anchor: Some(PathBuf::from(db_file.path())),
                config: Some(config_path),
            },
            true,
            Some(1),
            true,
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("--atomic rejected"),
            "unexpected atomic execution error: {error:#}"
        );
        assert_eq!(
            std::fs::read(&save_path).unwrap(),
            b"prior-complete-output\n"
        );
    }

    /// #1474: the user-facing `--atomic` executor prepares every operation
    /// before its commit pass. Two individually acyclic task writes can
    /// therefore form a cycle only inside the unit. The V16 commit-time
    /// guards must reject the later statement and roll the earlier one back
    /// for both authoritative dependency stores.
    #[tokio::test]
    async fn atomic_ops_file_rejects_same_unit_gtd_dependency_cycles() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let (a_id, b_id) = {
            let server = isolated_server(&db_path);
            let response = dispatch_json(
                &server,
                r#"[gtd.assign(title="AtomicCycleA", status="next"), gtd.assign(title="AtomicCycleB", status="next")]"#,
            )
            .await;
            (
                response["results"][0]["result"]["full_id"]
                    .as_str()
                    .expect("task A id")
                    .to_string(),
                response["results"][1]["result"]["full_id"]
                    .as_str()
                    .expect("task B id")
                    .to_string(),
            )
        };

        let compact_a_id = a_id.replace('-', "");
        let compact_b_id = b_id.replace('-', "");
        let alternate_spelling_error = crate::atomic_apply::execute_atomic_ops_file(
            vec![
                atomic_op(
                    "update",
                    serde_json::json!({
                        "id": a_id.clone(),
                        "properties": {"depends_on": [compact_b_id]}
                    }),
                ),
                atomic_op(
                    "update",
                    serde_json::json!({
                        "id": b_id.clone(),
                        "properties": {"depends_on": [compact_a_id]}
                    }),
                ),
            ],
            atomic_cfg(&db_path),
            &KhiveConfig::default(),
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("atomic preparation must reject an alternate dependency UUID spelling");
        let alternate_spelling_message = format!("{alternate_spelling_error:#}");
        assert!(
            alternate_spelling_message.contains("canonical lowercase hyphenated UUID"),
            "unexpected alternate-spelling error: {alternate_spelling_message}"
        );

        {
            let server = isolated_server(&db_path);
            let response = dispatch_json(&server, &format!(r#"get(id="{a_id}")"#)).await;
            assert!(
                response["results"][0]["result"]["properties"]
                    .get("depends_on")
                    .is_none(),
                "alternate dependency spelling must not persist: {response}"
            );
        }

        let property_envelope = crate::atomic_apply::execute_atomic_ops_file(
            vec![
                atomic_op(
                    "update",
                    serde_json::json!({
                        "id": a_id.clone(),
                        "properties": {"depends_on": [b_id.clone()]}
                    }),
                ),
                atomic_op(
                    "update",
                    serde_json::json!({
                        "id": b_id.clone(),
                        "properties": {"depends_on": [a_id.clone()]}
                    }),
                ),
            ],
            atomic_cfg(&db_path),
            &KhiveConfig::default(),
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("cycle is a clean atomic rollback, not a seam failure");
        assert_eq!(property_envelope["atomic"]["rolled_back"], true);
        assert_eq!(property_envelope["atomic"]["failed_op_index"], 1);
        assert!(
            property_envelope["atomic"]["error"]
                .as_str()
                .is_some_and(|error| error.contains("dependency cycle")),
            "envelope: {property_envelope}"
        );

        let server = isolated_server(&db_path);
        for task_id in [&a_id, &b_id] {
            let response = dispatch_json(&server, &format!(r#"get(id="{task_id}")"#)).await;
            assert!(
                response["results"][0]["result"]["properties"]
                    .get("depends_on")
                    .is_none(),
                "the earlier update must roll back too: {response}"
            );
        }

        let edge_envelope = crate::atomic_apply::execute_atomic_ops_file(
            vec![
                atomic_op(
                    "link",
                    serde_json::json!({
                        "source_id": a_id.clone(),
                        "target_id": b_id.clone(),
                        "relation": "depends_on"
                    }),
                ),
                atomic_op(
                    "link",
                    serde_json::json!({
                        "source_id": b_id.clone(),
                        "target_id": a_id.clone(),
                        "relation": "depends_on"
                    }),
                ),
            ],
            atomic_cfg(&db_path),
            &KhiveConfig::default(),
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("edge cycle is a clean atomic rollback, not a seam failure");
        assert_eq!(edge_envelope["atomic"]["rolled_back"], true);
        assert_eq!(edge_envelope["atomic"]["failed_op_index"], 1);
        assert!(
            edge_envelope["atomic"]["error"]
                .as_str()
                .is_some_and(|error| error.contains("dependency cycle")),
            "envelope: {edge_envelope}"
        );

        let server = isolated_server(&db_path);
        let response = dispatch_json(
            &server,
            &format!(r#"neighbors(id="{a_id}", direction="out", relations=["depends_on"])"#),
        )
        .await;
        assert_eq!(
            response["results"][0]["result"],
            serde_json::json!([]),
            "the earlier link must roll back too: {response}"
        );
    }

    /// ADR-099 B3 (second half): the inverse
    /// same-unit race — `[link(A, B, competes_with), update(X
    /// extends A-B -> competes_with)]`, where the CANONICAL row the update
    /// conflict-absorbs into is created by an EARLIER op in the SAME
    /// atomic unit (so it does not exist at either op's prepare time). The
    /// commit must both write correctly (X deleted, the just-linked row
    /// preserved unchanged per ADR-039 DO NOTHING — X's patch is discarded,
    /// not applied) and RENDER the correct surviving id — not X's
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
            envelope["results"][1]["result"]["weight"], 0.6,
            "ADR-039 DO NOTHING: the surviving row keeps its OWN pre-existing weight (0.6, \
             set by the link above), not the discarded update's patched weight (0.9): {envelope}"
        );

        let server = isolated_server(&db_path);
        let surviving_resp = dispatch_json(&server, &format!(r#"get(id="{linked_id}")"#)).await;
        assert_eq!(
            surviving_resp["results"][0]["result"]["weight"], 0.6,
            "the committed row itself must keep its pre-existing weight, not the discarded \
             update's patch: {surviving_resp}"
        );
    }

    /// The canonical survivor a
    /// symmetric-update op absorbs into can ALREADY be soft-deleted before the
    /// atomic unit even runs (not just tombstoned as a side effect of the same
    /// unit's own writes, as the sibling test above covers). This exercises
    /// `build_op_result`'s `get_edge_by_natural_key_including_deleted` call
    /// through the real atomic path with a genuinely pre-existing tombstone: the
    /// pre-fix renderer (`KhiveRuntime::list_edges`, which unconditionally
    /// filters `deleted_at IS NULL`) would report the committed update as "not
    /// found" for exactly this row, turning a successful DO NOTHING absorption
    /// into a spurious post-commit error.
    #[tokio::test]
    async fn atomic_symmetric_update_absorbs_into_pre_existing_tombstoned_survivor_and_renders_it()
    {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let (canonical_id, x_id) = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"[create(kind="concept", name="TombA"), create(kind="concept", name="TombB")]"#,
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

            // The canonical survivor: created, then soft-deleted, BEFORE the atomic
            // unit ever runs.
            let link_resp = dispatch_json(
                &server,
                &format!(
                    r#"link(source_id="{a_id}", target_id="{b_id}", relation="competes_with", weight=0.6)"#
                ),
            )
            .await;
            let canonical_id = link_resp["results"][0]["result"]["id"]
                .as_str()
                .expect("canonical id")
                .to_string();
            dispatch_json(&server, &format!(r#"delete(id="{canonical_id}")"#)).await;

            // A distinct pre-existing edge under a different relation, later
            // converted (by the atomic update below) into the same
            // (a, b, competes_with) natural key — it must absorb into the
            // already-tombstoned canonical row, not resurrect or overwrite it.
            let x_resp = dispatch_json(
                &server,
                &format!(
                    r#"link(source_id="{a_id}", target_id="{b_id}", relation="extends", weight=0.2)"#
                ),
            )
            .await;
            let x_id = x_resp["results"][0]["result"]["id"]
                .as_str()
                .expect("x id")
                .to_string();
            (canonical_id, x_id)
        };

        let ops = vec![atomic_op(
            "update",
            serde_json::json!({"id": x_id, "relation": "competes_with", "weight": 0.9}),
        )];

        let khive_cfg = KhiveConfig::default();
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("atomic run must succeed by absorbing into the tombstoned survivor");

        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );

        let rendered_id = envelope["results"][0]["result"]["id"]
            .as_str()
            .expect("update result id")
            .to_string();
        assert_eq!(
            rendered_id, canonical_id,
            "must render the pre-existing tombstoned canonical survivor, not X's stale \
             requested id: {envelope}"
        );
        assert!(
            !envelope["results"][0]["result"]["deleted_at"].is_null(),
            "the rendered survivor must show its OWN tombstoned state (non-null deleted_at) \
             — absorbing a conflicting update must not resurrect it: {envelope}"
        );
        assert_eq!(
            envelope["results"][0]["result"]["weight"], 0.6,
            "ADR-039 DO NOTHING: the survivor keeps its own pre-existing weight, not X's \
             discarded patched weight (0.9): {envelope}"
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

    /// `gtd.transition`: a typo'd key (`notee` for `note`) must be rejected
    /// before any write (task status unchanged); a well-formed transition
    /// still succeeds.
    #[tokio::test]
    async fn atomic_gtd_transition_unknown_field_rejected_well_formed_succeeds() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let task_id = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"gtd.assign(title="TransitionTypoGuard", status="inbox")"#,
            )
            .await;
            // gtd.assign's `id` field is always the short hex form
            // (handlers.rs:372) regardless of presentation mode — use
            // `full_id`, the real UUID, so it round-trips through the
            // atomic prepare path's UUID parse.
            resp["results"][0]["result"]["full_id"]
                .as_str()
                .expect("full_id")
                .to_string()
        };

        // (a) unknown field rejected — status must stay "inbox".
        let khive_cfg = KhiveConfig::default();
        let ops = vec![atomic_op(
            "gtd.transition",
            serde_json::json!({"id": task_id, "status": "next", "notee": "typo"}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("typo'd `notee` must be rejected");
        assert!(
            format!("{err:#}").contains("unknown field"),
            "error: {err:#}"
        );

        // (b) well-formed transition still succeeds.
        let ops = vec![atomic_op(
            "gtd.transition",
            serde_json::json!({"id": task_id, "status": "next"}),
        )];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("a well-formed gtd.transition must succeed");
        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );
    }

    /// `gtd.complete`: a typo'd key (`resutl` for `result`) must be
    /// rejected before any write (task status unchanged); a well-formed
    /// complete still succeeds.
    #[tokio::test]
    async fn atomic_gtd_complete_unknown_field_rejected_well_formed_succeeds() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();

        let task_id = {
            let server = isolated_server(&db_path);
            let resp = dispatch_json(
                &server,
                r#"gtd.assign(title="CompleteTypoGuard", status="next")"#,
            )
            .await;
            // Same `full_id` note as the transition test above.
            resp["results"][0]["result"]["full_id"]
                .as_str()
                .expect("full_id")
                .to_string()
        };

        // (a) unknown field rejected.
        let khive_cfg = KhiveConfig::default();
        let ops = vec![atomic_op(
            "gtd.complete",
            serde_json::json!({"id": task_id, "resutl": "typo"}),
        )];
        let err = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect_err("typo'd `resutl` must be rejected");
        assert!(
            format!("{err:#}").contains("unknown field"),
            "error: {err:#}"
        );

        // (b) well-formed complete still succeeds.
        let ops = vec![atomic_op(
            "gtd.complete",
            serde_json::json!({"id": task_id, "result": "shipped"}),
        )];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("a well-formed gtd.complete must succeed");
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
    /// `update` AND `gtd.transition` must succeed identically to canonical
    /// (which accepts full UUID or an 8+ hex prefix); a non-existent prefix
    /// must error with canonical's error shape ("no record matches
    /// prefix"). Pre-fix, atomic did a bare `Uuid::parse_str` and rejected
    /// any short id outright — the same ops-file that succeeds non-atomically
    /// (e.g. against `gtd.assign`'s own short `id` output) would fail before
    /// prepare under `--atomic`.
    #[tokio::test]
    async fn atomic_update_and_gtd_transition_accept_8_hex_prefix_ids() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let khive_cfg = KhiveConfig::default();

        let (entity_full_id, task_full_id) = {
            let server = isolated_server(&db_path);
            let resp =
                dispatch_json(&server, r#"create(kind="concept", name="PrefixEntity")"#).await;
            let entity_id = resp["results"][0]["result"]["id"]
                .as_str()
                .expect("entity id")
                .to_string();
            let resp =
                dispatch_json(&server, r#"gtd.assign(title="PrefixTask", status="next")"#).await;
            let task_id = resp["results"][0]["result"]["full_id"]
                .as_str()
                .expect("task full_id")
                .to_string();
            (entity_id, task_id)
        };
        let entity_prefix = &entity_full_id[..8];
        let task_prefix = &task_full_id[..8];

        // (a) 8-hex-prefix update and gtd.transition in the SAME atomic unit
        // both succeed.
        let ops = vec![
            atomic_op(
                "update",
                serde_json::json!({"id": entity_prefix, "name": "PrefixEntity-renamed"}),
            ),
            atomic_op(
                "gtd.transition",
                serde_json::json!({"id": task_prefix, "status": "active"}),
            ),
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
    /// `{ok, tool, op_index}`. Exercises all five v1-admissible verbs in one
    /// unit and asserts the relevant field for each:
    /// updated name for `update`, the deleted marker for `delete`, edge
    /// fields for `link`, and the transition/completion shape for the two
    /// gtd verbs.
    #[tokio::test]
    async fn atomic_success_results_carry_canonical_shaped_result_per_op() {
        let db_file = NamedTempFile::new().expect("temp db");
        let db_path = db_file.path().to_str().expect("utf8").to_string();
        let khive_cfg = KhiveConfig::default();

        // `transition_task_id` and `complete_task_id` are DELIBERATELY two
        // separate tasks, not one task chained through both verbs: every
        // op's prepare pass reads state BEFORE the atomic unit applies any
        // statement (ADR-099 D1 — prepare is async/read-only, commit is the
        // one synchronous pass), so a `gtd.transition` and a `gtd.complete`
        // on the SAME task in the SAME unit would race against each other's
        // as-yet-uncommitted write, not compose sequentially.
        let (entity_id, doomed_id, source_id, target_id, transition_task_id, complete_task_id) = {
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
            let resp = dispatch_json(
                &server,
                r#"gtd.assign(title="ResultTransitionTask", status="next")"#,
            )
            .await;
            let transition_task_id = resp["results"][0]["result"]["full_id"]
                .as_str()
                .expect("task full_id")
                .to_string();
            let resp = dispatch_json(
                &server,
                r#"gtd.assign(title="ResultCompleteTask", status="active")"#,
            )
            .await;
            let complete_task_id = resp["results"][0]["result"]["full_id"]
                .as_str()
                .expect("task full_id")
                .to_string();
            (
                id(0),
                id(1),
                id(2),
                id(3),
                transition_task_id,
                complete_task_id,
            )
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
            atomic_op(
                "gtd.transition",
                serde_json::json!({"id": transition_task_id, "status": "active"}),
            ),
            atomic_op(
                "gtd.complete",
                serde_json::json!({"id": complete_task_id, "result": "shipped"}),
            ),
        ];
        let envelope = crate::atomic_apply::execute_atomic_ops_file(
            ops,
            atomic_cfg(&db_path),
            &khive_cfg,
            khive_types::pack::ATOMIC_MAX_OPS_DEFAULT,
        )
        .await
        .expect("all five v1-admissible verbs must commit as one unit");
        assert_eq!(
            envelope["atomic"]["committed"], true,
            "envelope: {envelope}"
        );

        let results = envelope["results"].as_array().expect("results array");
        assert_eq!(results.len(), 5, "envelope: {envelope}");

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

        assert_eq!(
            results[3]["result"]["transitioned"], true,
            "gtd.transition result: {envelope}"
        );
        assert_eq!(
            results[3]["result"]["to"], "active",
            "gtd.transition result must carry the new status: {envelope}"
        );

        assert_eq!(
            results[4]["result"]["completed"], true,
            "gtd.complete result: {envelope}"
        );
        assert_eq!(
            results[4]["result"]["to"], "done",
            "gtd.complete result must carry the terminal status: {envelope}"
        );
    }
}
