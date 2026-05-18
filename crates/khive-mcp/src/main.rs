use std::path::PathBuf;

use clap::Parser;
use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{KhiveRuntime, RuntimeConfig};

#[derive(Parser, Debug)]
#[command(
    name = "khive-mcp",
    version,
    about = "khive MCP server (stdio) — the only user-facing Rust binary"
)]
struct Args {
    /// Path to the khive database. Use \":memory:\" for an ephemeral in-memory database.
    #[arg(long, env = "KHIVE_DB")]
    db: Option<String>,

    /// Default namespace for operations that do not specify one.
    #[arg(long, env = "KHIVE_NAMESPACE", default_value = "local")]
    namespace: String,

    /// Disable local embedding model (skips vector indexing on create/update).
    #[arg(long, env = "KHIVE_NO_EMBED")]
    no_embed: bool,

    /// Log level for stderr output (stdout is reserved for the MCP protocol).
    #[arg(long, env = "KHIVE_LOG", default_value = "warn")]
    log: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Tracing goes to stderr — stdout is MCP JSON-RPC.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(args.log.clone())
        .with_ansi(false)
        .init();

    let db_path = match args.db.as_deref() {
        Some(":memory:") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            Some(PathBuf::from(format!("{home}/.khive/khive-graph.db")))
        }
    };

    let embedding_model = if args.no_embed {
        None
    } else {
        RuntimeConfig::default().embedding_model
    };

    let config = RuntimeConfig {
        db_path,
        default_namespace: args.namespace,
        embedding_model,
        ..RuntimeConfig::default()
    };

    let runtime = KhiveRuntime::new(config)?;
    let server = KhiveMcpServer::new(runtime);
    server.serve_stdio().await?;
    Ok(())
}
