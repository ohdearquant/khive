//! `kkernel exec` — run a verb DSL expression directly through the pack registry.
//!
//! When the warm daemon is reachable, exec forwards through it instead of
//! building an in-process runtime (ADR-049). Config and namespace are matched
//! against the daemon's own fingerprint; a mismatch falls back to local
//! dispatch, keeping behaviour identical to the in-process path.
//! Accepted daemon results disclose that logging is configured separately:
//! `--log` and `KHIVE_LOG` affect the client process, while the daemon's level
//! is fixed at startup. The response protocol does not report a daemon PID
//! or stderr destination, so this disclosure includes neither.
//! A forwarded lexical-timeout response is also reported as a client-side
//! WARN: the daemon's detailed event cannot reach the caller's stderr.
//!
//! ## Modes
//!
//! - **DSL mode** (default): `kkernel exec '<dsl>'` — executes a single verb DSL
//!   expression or batch against the configured database and namespace.
//! - **Plan mode**: `kkernel exec --plan '<dsl>'` — parses through an already
//!   running daemon and prints its plan without dispatch or local construction.
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
//! `--serial` keeps the same logical chunks and one warm server/model instance,
//! but runs each full chunk through the same batch path with handler concurrency
//! capped at one. The default remains bounded parallel execution; `--serial`
//! conflicts with `--atomic`.
//! `--dry-run` validates every line and prints a per-verb summary without writes.

use std::io::{Seek as _, Write as _};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

#[cfg(test)]
use khive_mcp::serve::resolve_runtime_config;
use khive_mcp::serve::{
    apply_env_output_format, build_server_multi_backend_with_db_anchor,
    build_single_backend_runtime, config_discovery_db_anchor, enforce_strict_actor_mode,
    normalize_redundant_db_override_with_source, reject_conflicting_db_override_with_source,
    validate_declared_backend_access_modes, validate_wal_ceiling_topology, RuntimeConfigInputs,
};
use khive_mcp::server::KhiveMcpServer;
#[cfg(unix)]
use khive_mcp::server::{compute_config_id, compute_config_id_with_storage_mode};
use khive_mcp::tools::request::RequestParams;
#[cfg(test)]
use khive_runtime::KhiveRuntime;
#[cfg(unix)]
use khive_runtime::{daemon::PROTOCOL_VERSION, DaemonRequestFrame};
use khive_runtime::{KhiveConfig, Namespace, RuntimeConfig};
use khive_types::RefusalReason;

mod args;
mod ops_file_apply;
mod ops_file_validate;
mod plan;
mod refusal;

#[cfg(test)]
use ops_file_apply::{
    apply_ops_file, apply_ops_file_with_dispatch_mode, apply_ops_file_with_response_transform,
    collect_op_failures, ops_file_progress_line, ops_file_summary, retain_failure_detail,
    validate_ordered_chunk_envelope, AbortedOpsFileError, OpsFileReportMode,
};
use ops_file_apply::{
    apply_ops_file_reader_with_dispatch_mode, should_defer_chunk_entry, OpsFileDispatchMode,
};

#[cfg(test)]
pub(crate) use ops_file_validate::parse_ops_file;
#[cfg(test)]
use ops_file_validate::read_bounded_ops_line_with_limit;
pub(crate) use ops_file_validate::OpsFileEntry;
use ops_file_validate::{
    parse_atomic_validated_snapshot, parse_ops_file_line, preflight_typed_validated_snapshot,
    read_bounded_ops_line, validate_ops_file, validated_tool_names,
};

pub(crate) use refusal::acquire_local_construction_guard;
#[cfg(all(test, unix))]
use refusal::ForwardFuture;
use refusal::{
    emit_refusal, refusal_error, report_invocation_refusal, report_tools_refusal,
    report_unscoped_refusal, ExecRefusal,
};
#[cfg(unix)]
use refusal::{forward_or_spawn_boxed, ForwardFnPtr};

pub use args::ExecArgs;

// The scheduled-event drain now lives in `khive-mcp` (ADR-106: the
// daemon-resident tick needs to call it from `khive-mcp::serve`, which
// cannot depend back on `kkernel`).
use khive_mcp::pending_events;

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
    if args.plan {
        let result = plan::run(&args).await?;
        writeln!(std::io::stdout().lock(), "{result}")?;
        return Ok(());
    }

    // Clap enforces these relations for normal CLI entry. Keep the same
    // boundary for library callers that construct `ExecArgs` directly.
    if args.serial && (args.ops_file.is_none() || args.ops.is_some() || args.atomic) {
        anyhow::bail!(
            "--serial requires --ops-file and conflicts with positional ops and --atomic"
        );
    }

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

    // ADR-170 embedded mode: `kkernel exec`'s in-process fallback is a
    // one-shot — a socket forwarder would be reaped at process exit before
    // delivering, losing every event. The shared resolver already emits
    // direct (socket-less) mode for exactly this class of host; only the
    // resident daemon entrypoints upgrade to forwarding
    // (`enable_events_forwarding_for_daemon`). SQLite's per-file
    // cross-process exclusion covers direct appends overlapping a running
    // events daemon.
    debug_assert!(cfg
        .events_split
        .as_ref()
        .is_none_or(|split| split.socket_path.is_none()));

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
                args.serial,
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
fn disclose_resolved_database(cfg: &RuntimeConfig, khive_cfg: &KhiveConfig, force_memory: bool) {
    use std::io::Write;
    let line =
        khive_mcp::serve::resolved_database_disclosure(cfg.db_path.as_deref(), &khive_cfg.backends);
    let _ = writeln!(std::io::stderr(), "{line}");
    let lock_line =
        khive_mcp::serve::resolved_volume_lock_disclosure(cfg.volume_lock_dir.as_deref());
    let _ = writeln!(std::io::stderr(), "{lock_line}");
    let wal_line =
        khive_mcp::serve::resolved_wal_ceiling_disclosure(cfg, &khive_cfg.backends, force_memory);
    let _ = writeln!(std::io::stderr(), "{wal_line}");
}

fn disclose_resolved_actor(cfg: &RuntimeConfig) {
    use std::io::Write;
    let line = khive_mcp::serve::resolved_actor_disclosure(cfg.actor_id.as_deref());
    let _ = writeln!(std::io::stderr(), "{line}");
}

#[cfg(unix)]
fn disclose_daemon_execution() {
    use std::io::Write;
    let _ = writeln!(
        std::io::stderr(),
        "execution: answered by daemon; --log and KHIVE_LOG set the client process log level only; \
         the daemon log level is fixed at startup"
    );
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

/// Bound on how long [`settle_exec_storage_before_return`] waits for one
/// writer-task join. Matches the bound the settle-contract test at
/// `khive-mcp/src/serve.rs` asserts on the identical join.
const EXEC_STORAGE_SETTLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// Settle a short-lived exec invocation's storage before its
/// [`KhiveMcpServer`] is dropped, so the last connection SQLite closes on
/// this database is always writable and can checkpoint the WAL away
/// (#3089). Every failure here is logged and swallowed: this never changes
/// the caller's returned result. `code_ingest.rs`'s own writer-task-join
/// drain (`settle_writer_drain`) instead fails the whole call when its
/// ingest succeeded and the drain then timed out, because that command
/// documents "return implies settled". These exec dispatch paths make no
/// such durability promise today, so this settle stays advisory: bail on a
/// stuck writer task here would be a new exec failure mode with no
/// existing contract requiring it.
///
/// Order matters, and mirrors the settle-contract test at
/// `khive-mcp/src/serve.rs` (take joins, drop the owner, await
/// the joins) plus `code_ingest.rs`'s reason for awaiting a
/// writer-task join at all before returning:
///
/// 1. Drain the ADR-133 audit-batch supervisor first
///    (`KhiveMcpServer::shutdown_audit_batch`), following the shutdown
///    order `VerbRegistry::shutdown_audit_batch` documents. The supervisor
///    is a detached task that holds a clone of the audit `EventStore`
///    while it commits already-accepted rows and exits once its queue
///    drains. Draining it here means no such clone outlives this call, so
///    dropping the server in step 3 releases every storage reference the
///    server's registry held.
/// 2. Take every pool's writer-task join BEFORE dropping the server: once
///    the server and every local clone taken here are gone, there is no
///    live reference left to reach the field through.
/// 3. Drop the server. With the supervisor's clone released in step 1 and
///    no local clones outstanding from step 2, this drops the server's own
///    `Arc<ConnectionPool>` clones. If those were a pool's last
///    references, `ConnectionPool::drop` (`crates/khive-db/src/pool.rs`)
///    runs here: it closes every read-only reader before its own writer
///    connection, so a reader is never that pool's own last closer, and
///    the writer-task's channel sender (the pool's last field to drop) is
///    what actually signals the writer task to exit.
/// 4. Await each taken join, bounded. Dropping the sender in step 3 only
///    signals the writer task, a separate `tokio::spawn`ed task owning its
///    own standalone writable connection, to exit; it still has to run and
///    close that connection on its own schedule. Awaiting it here,
///    inside this still-running executor, is what makes that close happen
///    in this sequence instead of racing Tokio runtime shutdown, which
///    drops idle tasks in random per-worker order.
///
/// Together, steps 3 and 4 mean the only two connections capable of
/// closing last on this database (a pool's own writer connection and its
/// writer task's standalone connection) are both writable; neither is ever
/// a read-only reader, so the last real closer can always take the
/// EXCLUSIVE lock SQLite needs to checkpoint.
async fn settle_exec_storage_before_return(server: KhiveMcpServer) {
    if let Err(reason) = server.shutdown_audit_batch().await {
        tracing::warn!(
            reason = ?reason,
            "exec: audit-batch drain did not complete cleanly before storage settle"
        );
    }

    let mut writer_task_joins = Vec::new();
    if let Some(pool) = server.pool() {
        if let Some(join) = pool.take_writer_task_join() {
            writer_task_joins.push(join);
        }
    }
    for pool in server.secondary_pools() {
        if let Some(join) = pool.take_writer_task_join() {
            writer_task_joins.push(join);
        }
    }

    drop(server);

    for join in writer_task_joins {
        match tokio::time::timeout(EXEC_STORAGE_SETTLE_TIMEOUT, join).await {
            Ok(Ok(())) => {}
            Ok(Err(error)) => tracing::warn!(
                %error,
                "exec: a writer task panicked while settling storage before return"
            ),
            Err(_) => tracing::warn!(
                timeout = ?EXEC_STORAGE_SETTLE_TIMEOUT,
                "exec: a writer task did not settle before returning"
            ),
        }
    }
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
        // Declared storage replaces the construction anchor even when --db
        // was omitted. Keep the captured anchor in step with that deliberate
        // normalization before the local fallback checks for path drift.
        db_context.anchor = cfg.db_path.clone();
        force_memory
    };

    if !force_memory {
        validate_declared_backend_access_modes(&khive_cfg.backends)?;
    }
    validate_wal_ceiling_topology(&cfg, &khive_cfg.backends, force_memory)?;

    disclose_resolved_database(
        &cfg,
        &khive_cfg,
        db_context.raw.as_deref() == Some(":memory:"),
    );
    disclose_resolved_actor(&cfg);

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
            plan: false,
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
        //   withhold it: both sides use the config-declared main path and the
        //   override adds no storage selection information.
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
        // Forward this client's already-resolved pack list unconditionally so
        // a daemon this call spawns matches `cfg`'s `config_id` fingerprint
        // (which folds `packs`) instead of re-deriving its own selection from
        // ambient env/config and risking a mismatch (khive-oss#1941).
        let spawn_packs = cfg.packs.clone();
        if let Some(res) = forward_fn(&frame, spawn_config, spawn_db, spawn_packs).await {
            let output = res.map_err(|e| anyhow::anyhow!("{}", e.message))?;
            disclose_daemon_execution();
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
    )
    .await?;

    let params = RequestParams {
        plan: None,
        ops,
        presentation,
        presentation_per_op: None,
        save_to: save_file,
        // Tier-1: CLI --output-format overrides the server default (env/builtin).
        format: output_format,
        format_per_op: None,
        request_id: None,
    };

    let dispatch_result = server
        .dispatch_request_local_for_exec(params, strict)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"));

    // Every branch below reaches the same tail: settle storage before this
    // function (and, for a daemonless invocation, the process) returns,
    // regardless of whether dispatch or the strict-batch check failed.
    let final_result = match dispatch_result {
        Ok(raw_output) => {
            let output = prepare_exec_output(&raw_output, strict);
            println!("{output}");
            enforce_strict_batch_result(&output, strict)
        }
        Err(error) => Err(error),
    };
    settle_exec_storage_before_return(server).await;
    final_result
}

/// Build the server used whenever `kkernel exec` dispatches a request locally
/// instead of through the warm daemon (both the fallback and `--ops-file`
/// bulk-apply paths). See
/// `crates/kkernel/docs/design.md#exec-local-dispatch-fallback-server-adr-067-adr-028-8`
/// for why this must agree with the daemon's own multi-backend boot logic.
async fn build_local_fallback_server(
    cfg: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
) -> Result<KhiveMcpServer> {
    // Held across the real-host async schema/application coordinator and pack
    // assembly; dropped only after the fully wired server is ready.
    let _boot_guard = acquire_local_construction_guard(&cfg)?;
    if khive_cfg.backends.is_empty() {
        let rt = build_single_backend_runtime(cfg, khive_cfg).await?;
        let env_fmt = apply_env_output_format(khive_cfg.runtime.default_output_format);
        Ok(KhiveMcpServer::new_with_mounts(rt)
            .await
            .map_err(|e| anyhow::anyhow!("{e}"))?
            .with_default_output_format(env_fmt))
    } else {
        build_server_multi_backend_with_db_anchor(cfg, khive_cfg, cli_db_override, db_anchor).await
    }
}

struct AtomicSavePublishFailure {
    stdout: String,
    error: anyhow::Error,
}

/// Render the authoritative atomic stdout value. A save sink is preflighted
/// before execution, but its writes/flush/rename necessarily happen after the
/// database outcome is known. If that publication fails after a commit, keep
/// the process failure while returning a reconciliation envelope that makes
/// the durable, non-retryable outcome explicit.
fn render_atomic_output(
    envelope: &mut serde_json::Value,
    save_sink: Option<khive_mcp::save_sink::JsonlSaveSink>,
) -> std::result::Result<String, AtomicSavePublishFailure> {
    let Some(save_sink) = save_sink else {
        return Ok(serde_json::to_string_pretty(envelope).expect("serialize atomic envelope"));
    };

    match save_sink.write_envelope(envelope) {
        Ok(manifest) => {
            Ok(serde_json::to_string(&manifest).expect("serialize atomic save manifest"))
        }
        Err(error) => {
            let committed = crate::atomic_apply::record_save_file_publish_failure(envelope, &error);
            let stdout = serde_json::to_string_pretty(envelope)
                .expect("serialize atomic save failure reconciliation envelope");
            let error = if committed {
                error.context(
                    "atomic database changes committed but --save-file publication failed; \
                     do not replay the mutation (inspect stdout for reconciliation details)",
                )
            } else {
                error.context("atomic --save-file publication failed")
            };
            Err(AtomicSavePublishFailure { stdout, error })
        }
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
    serial: bool,
    atomic: bool,
    atomic_max_ops: Option<usize>,
    strict: bool,
) -> Result<()> {
    if serial && atomic {
        anyhow::bail!("--serial conflicts with --atomic");
    }

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

    if serial {
        if let Err(error) =
            preflight_typed_validated_snapshot(&mut validated.snapshot, validated.total)
        {
            if let Some(refusal) = error.downcast_ref::<ExecRefusal>() {
                return Err(report_unscoped_refusal(
                    refusal.reason,
                    refusal.message.as_str(),
                ));
            }
            return Err(error);
        }
    }

    if dry_run && !atomic {
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

    disclose_resolved_database(
        &cfg,
        &khive_cfg,
        db_context.raw.as_deref() == Some(":memory:"),
    );
    disclose_resolved_actor(&cfg);

    if atomic {
        let max_ops = atomic_max_ops.unwrap_or(khive_types::pack::ATOMIC_MAX_OPS_DEFAULT);
        let ops =
            parse_atomic_validated_snapshot(&mut validated.snapshot, validated.total, max_ops)?;
        if dry_run {
            if let Err(error) =
                crate::atomic_apply::preflight_atomic_ops_file(&ops, &cfg, &khive_cfg, max_ops)
            {
                if let Some(failure) =
                    error.downcast_ref::<crate::atomic_apply::AtomicExecFailure>()
                {
                    let mut envelope = failure.envelope();
                    annotate_and_emit_refusals(&mut envelope, strict);
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&envelope)
                            .expect("serialize atomic dry-run refusal envelope")
                    );
                }
                return Err(error);
            }
            let summary = serde_json::json!({
                "dry_run": true,
                "atomic": true,
                "total": validated.total,
                "per_verb": validated.per_verb,
            });
            println!(
                "{}",
                serde_json::to_string_pretty(&summary).expect("serialize atomic dry-run summary")
            );
            return Ok(());
        }
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
        let output = match render_atomic_output(&mut envelope, save_sink) {
            Ok(output) => output,
            Err(failure) => {
                // stdout is the machine reconciliation channel. Emit it before
                // returning the non-zero sink error so callers never infer that
                // silence means the atomic database unit is safe to replay.
                println!("{}", failure.stdout);
                return Err(failure.error);
            }
        };
        println!("{output}");
        if envelope["atomic"]["rolled_back"].as_bool() == Some(true) {
            anyhow::bail!("atomic unit rolled back; inspect stdout for the failed operation");
        }
        return Ok(());
    }

    let server = build_local_fallback_server(
        cfg,
        &khive_cfg,
        db_context.raw.as_deref(),
        db_context.anchor.as_deref(),
    )
    .await?;

    validated
        .snapshot
        .rewind()
        .context("rewind validated ops-file snapshot for dispatch")?;
    let dispatch_result = apply_ops_file_reader_with_dispatch_mode(
        &server,
        std::io::BufReader::new(validated.snapshot),
        validated.total,
        presentation,
        output_format,
        save_file,
        strict,
        if serial {
            OpsFileDispatchMode::Serial
        } else {
            OpsFileDispatchMode::BoundedParallel
        },
    )
    .await
    .map(|_| ());

    // Settle storage before this function (and, for a daemonless
    // invocation, the process) returns, regardless of dispatch outcome.
    settle_exec_storage_before_return(server).await;
    dispatch_result
}

#[cfg(test)]
#[path = "exec_tests.rs"]
mod tests;
