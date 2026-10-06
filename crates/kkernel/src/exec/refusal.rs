#[cfg(unix)]
use std::path::PathBuf;

use anyhow::{Context, Result};
#[cfg(unix)]
use khive_runtime::DaemonRequestFrame;
use khive_runtime::RuntimeConfig;
use khive_types::RefusalReason;

/// Stable stderr prefix for machine-classifiable exec refusals.
const REFUSAL_PREFIX: &str = "kkernel-refusal: ";

#[derive(Debug)]
pub(super) struct ExecRefusal {
    pub(super) reason: RefusalReason,
    pub(super) message: String,
}

impl std::fmt::Display for ExecRefusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ExecRefusal {}

pub(super) fn refusal_error(reason: RefusalReason, message: impl Into<String>) -> anyhow::Error {
    anyhow::Error::new(ExecRefusal {
        reason,
        message: message.into(),
    })
}

pub(super) fn emit_refusal(reason: RefusalReason) {
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
pub(super) fn report_unscoped_refusal(
    reason: RefusalReason,
    message: impl Into<String>,
) -> anyhow::Error {
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
pub(super) fn report_invocation_refusal(
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

pub(super) fn report_tools_refusal(
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
pub(super) type ForwardFuture<'a> = std::pin::Pin<
    Box<dyn std::future::Future<Output = Option<Result<String, rmcp::ErrorData>>> + Send + 'a>,
>;

/// Function pointer type for the daemon-forwarding seam.
#[cfg(unix)]
pub(super) type ForwardFnPtr = for<'a> fn(
    &'a DaemonRequestFrame,
    Option<PathBuf>,
    Option<&'a str>,
    Vec<String>,
) -> ForwardFuture<'a>;

/// Adapts the real `forward_or_spawn_with_config_and_packs` to the `ForwardFnPtr` signature.
#[cfg(unix)]
pub(super) fn forward_or_spawn_boxed<'a>(
    frame: &'a DaemonRequestFrame,
    config: Option<PathBuf>,
    db: Option<&'a str>,
    packs: Vec<String>,
) -> ForwardFuture<'a> {
    Box::pin(async move {
        khive_mcp::daemon::forward_or_spawn_with_config_and_packs(
            frame,
            config.as_deref(),
            db,
            Some(&packs),
        )
        .await
    })
}

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
/// Uses `std::fs::File::lock()` (stabilized 1.89, workspace MSRV 1.95) on the
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
