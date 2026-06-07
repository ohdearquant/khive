//! `kkernel exec` — run a verb DSL expression directly through the pack registry.

use anyhow::Result;
use clap::Parser;

use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
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

/// Execute the DSL expression in-process and print the JSON result to stdout.
pub async fn run_exec(args: ExecArgs) -> Result<()> {
    let mut cfg = RuntimeConfig::default();
    if let Some(db_path) = resolve_db_override(args.db.as_deref()) {
        cfg.db_path = db_path;
    }
    cfg.default_namespace =
        Namespace::parse(&args.namespace).map_err(|e| anyhow::anyhow!("{e}"))?;

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
