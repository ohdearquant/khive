//! `kkernel exec` — run a verb DSL expression directly through the pack registry.
//!
//! When the warm daemon is reachable, exec forwards through it instead of
//! building an in-process runtime (ADR-049). Config and namespace are matched
//! against the daemon's own fingerprint; a mismatch falls back to local
//! dispatch, keeping behaviour identical to the in-process path.

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
    pub ops: String,

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
            ops: args.ops.clone(),
            presentation: args.presentation.clone(),
            presentation_per_op: None,
            namespace: cfg.default_namespace.as_str().to_string(),
            config_id: compute_config_id(&cfg),
            protocol_version: PROTOCOL_VERSION,
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
        ops: args.ops,
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
}
