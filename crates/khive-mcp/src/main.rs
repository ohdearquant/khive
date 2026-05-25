use std::path::PathBuf;

use clap::Parser;
use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{
    config_from_env, runtime_config_from_khive_config, KhiveConfig, KhiveRuntime, RuntimeConfig,
};

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

    /// Pack to load into the verb registry. Repeat for multiple
    /// (e.g. `--pack kg --pack gtd`). Falls back to `KHIVE_PACKS` env
    /// (comma- or whitespace-separated) or `["kg"]` if neither is set.
    #[arg(long = "pack")]
    pack: Vec<String>,

    /// Path to a khive TOML config file.
    ///
    /// When provided, embedding engine configuration is loaded from
    /// `[[engines]]` blocks in this file. Overrides env vars
    /// `KHIVE_EMBEDDING_MODEL` and `KHIVE_ADDITIONAL_EMBEDDING_MODELS`.
    ///
    /// Default search path when this flag is absent: `.khive/config.toml`
    /// relative to the server's working directory (project-local).
    /// If neither the flag path nor the default path exists, the env-var
    /// embedding config path is used for backward compatibility.
    #[arg(long = "config", env = "KHIVE_CONFIG")]
    config: Option<PathBuf>,
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

    // CLI `--pack` overrides env-derived default. Empty means "use default".
    let packs = if args.pack.is_empty() {
        RuntimeConfig::default().packs
    } else {
        args.pack
    };

    let default_namespace = khive_runtime::Namespace::parse(&args.namespace)
        .map_err(|e| anyhow::anyhow!("invalid --namespace {:?}: {e}", args.namespace))?;

    // Build base config before embedding engine resolution.
    let base_config = RuntimeConfig {
        db_path,
        default_namespace,
        packs,
        ..RuntimeConfig::default()
    };

    // Resolve embedding engine configuration from config file or env vars.
    let config = if args.no_embed {
        // --no-embed takes priority: zero out embedding.
        RuntimeConfig {
            embedding_model: None,
            additional_embedding_models: vec![],
            ..base_config
        }
    } else {
        resolve_embedding_config(args.config.as_deref(), base_config)?
    };

    let runtime = KhiveRuntime::new(config)?;
    let server = KhiveMcpServer::new(runtime).map_err(|e| anyhow::anyhow!("{e}"))?;
    server.serve_stdio().await?;
    Ok(())
}

/// Resolve the final embedding config from the config file or env-var fallback.
///
/// Precedence:
/// 1. Config file (from `--config` / `KHIVE_CONFIG` / `~/.khive/config.toml`)
/// 2. Env-var fallback (`KHIVE_EMBEDDING_MODEL` + `KHIVE_ADDITIONAL_EMBEDDING_MODELS`)
///
/// If both the file and env vars are present, the file wins and a warning is
/// emitted about ignored env vars.
fn resolve_embedding_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
) -> anyhow::Result<RuntimeConfig> {
    match KhiveConfig::load(config_path).map_err(|e| anyhow::anyhow!("config error: {e}"))? {
        Some(khive_cfg) => {
            // Config file present — check if env vars are also set and warn.
            let env_primary = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
            let env_additional = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS").ok();
            if env_primary.is_some() || env_additional.is_some() {
                tracing::warn!(
                    "khive.toml config file is present; KHIVE_EMBEDDING_MODEL and \
                     KHIVE_ADDITIONAL_EMBEDDING_MODELS env vars are ignored"
                );
            }
            Ok(runtime_config_from_khive_config(&khive_cfg, base))
        }
        None => {
            // No config file — fall back to env-var path.
            let env_cfg = config_from_env();
            if env_cfg.engines.is_empty() {
                // No env vars either; use the base config as-is (includes
                // RuntimeConfig::default()'s env-var embedding model).
                Ok(base)
            } else {
                Ok(runtime_config_from_khive_config(&env_cfg, base))
            }
        }
    }
}
