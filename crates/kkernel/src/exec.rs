//! `kkernel exec` — run a verb DSL expression directly through the pack registry.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

use khive_mcp::server::KhiveMcpServer;
use khive_mcp::tools::request::RequestParams;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};

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
/// Resolve the `--db`/`KHIVE_DB` value into a `db_path` override, mirroring
/// `kkernel mcp`: an explicit `:memory:` means the ephemeral in-memory db
/// (`None`), not a file literally named ":memory:" (which SQLite treats as a
/// per-connection file → empty schema). `None` leaves the default in place.
fn resolve_db_override(db: Option<&str>) -> Option<Option<PathBuf>> {
    match db {
        Some(":memory:") => Some(None),
        Some(path) => Some(Some(PathBuf::from(path))),
        None => None,
    }
}

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
    fn memory_sentinel_maps_to_none() {
        // `:memory:` must become db_path=None (ephemeral), not a file path.
        assert_eq!(resolve_db_override(Some(":memory:")), Some(None));
    }

    #[test]
    fn explicit_path_maps_to_some() {
        assert_eq!(
            resolve_db_override(Some("/tmp/kkernel-exec-test.db")),
            Some(Some(PathBuf::from("/tmp/kkernel-exec-test.db")))
        );
    }

    #[test]
    fn absent_db_leaves_default() {
        // None → no override; run_exec keeps RuntimeConfig::default().db_path.
        assert_eq!(resolve_db_override(None), None);
    }

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
