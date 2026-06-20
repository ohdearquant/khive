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

use anyhow::Result;
use clap::Parser;

#[cfg(unix)]
use khive_mcp::server::compute_config_id;
use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
#[cfg(unix)]
use khive_runtime::{daemon::PROTOCOL_VERSION, DaemonRequestFrame};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};

use crate::dbpath::resolve_db_override;
use crate::pending_events;

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
    /// Mutually exclusive with `--pending-events`.
    pub ops: Option<String>,

    /// One-shot drain: fire all `scheduled_event` notes whose `trigger_at <= now`.
    ///
    /// Scans all namespaces, dispatches each event's action in its own namespace,
    /// marks fired events, and advances repeating events (daily/weekly/monthly).
    /// Prints a JSON summary of scanned/fired/advanced/failed counts to stdout.
    ///
    /// Mutually exclusive with the positional `ops` argument. Suitable for cron:
    ///   * * * * *  kkernel exec --pending-events
    #[arg(long, conflicts_with = "ops")]
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
pub async fn run_exec(args: ExecArgs) -> Result<()> {
    // ── pending-events drain ─────────────────────────────────────────────────
    if args.pending_events {
        let summary =
            pending_events::run_pending_events(args.db.as_deref(), &args.namespace, args.verbose)
                .await?;
        pending_events::print_summary(&summary);
        return Ok(());
    }

    // ── DSL mode ─────────────────────────────────────────────────────────────
    let ops = match args.ops {
        Some(s) => s,
        None => {
            anyhow::bail!(
                "kkernel exec: provide a DSL expression (positional) or --pending-events"
            );
        }
    };

    let mut cfg = RuntimeConfig::default();
    if let Some(db_path) = resolve_db_override(args.db.as_deref()) {
        cfg.db_path = db_path;
    }
    cfg.default_namespace =
        Namespace::parse(&args.namespace).map_err(|e| anyhow::anyhow!("{e}"))?;

    // ── daemon fast-path (Unix only) ─────────────────────────────────────────
    #[cfg(unix)]
    {
        let frame = DaemonRequestFrame {
            ops: ops.clone(),
            presentation: args.presentation.clone(),
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
    let server = KhiveMcpServer::new(rt).map_err(|e| anyhow::anyhow!("{e}"))?;

    let params = RequestParams {
        ops,
        presentation: args.presentation,
        presentation_per_op: None,
    };

    let output = server
        .dispatch_request_local(params)
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    println!("{output}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;
    use serial_test::serial;

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
    fn ops_positional_works_without_pending_events() {
        let args = ExecArgs::parse_from(["exec", "stats()"]);
        assert_eq!(args.ops.as_deref(), Some("stats()"));
        assert!(!args.pending_events);
    }
}
