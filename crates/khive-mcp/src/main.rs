//! khive-mcp binary entry point — parses CLI args, builds a `KhiveRuntime`,
//! and serves over stdio (or runs as a daemon with `--daemon` on Unix).

use std::path::PathBuf;

use clap::Parser;
use khive_mcp::args::{resolve_cli_namespace, Args};
use khive_mcp::server::KhiveMcpServer;
use khive_runtime::{
    config_from_env, runtime_config_from_khive_config, KhiveConfig, KhiveRuntime, RuntimeConfig,
};

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

    // Namespace resolution — see resolve_cli_namespace in khive_mcp::args for semantics.
    let (cli_namespace_explicit, cli_namespace) =
        resolve_cli_namespace(&args).map_err(|e| anyhow::anyhow!("{e}"))?;

    // CLI `--pack` overrides env-derived default. Empty means "use default".
    let packs = if args.pack.is_empty() {
        RuntimeConfig::default().packs
    } else {
        args.pack
    };

    // Build base config before embedding engine resolution.
    let base_config = RuntimeConfig {
        db_path,
        default_namespace: cli_namespace,
        packs,
        ..RuntimeConfig::default()
    };

    // Resolve full config: embedding engines + actor namespace from config file.
    let config = if args.no_embed {
        // --no-embed takes priority: zero out embedding.
        // Still apply config-file actor if no CLI actor was given.
        let no_embed_base = RuntimeConfig {
            embedding_model: None,
            additional_embedding_models: vec![],
            ..base_config
        };
        resolve_actor_from_config(
            args.config.as_deref(),
            no_embed_base,
            cli_namespace_explicit,
        )?
    } else {
        resolve_config(args.config.as_deref(), base_config, cli_namespace_explicit)?
    };

    let runtime = KhiveRuntime::new(config)?;
    let server = KhiveMcpServer::new(runtime).map_err(|e| anyhow::anyhow!("{e}"))?;
    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon(server).await?;
        return Ok(());
    }
    #[cfg(not(unix))]
    if args.daemon {
        anyhow::bail!("--daemon mode requires Unix (macOS/Linux). On Windows, khive-mcp runs in stdio mode only.");
    }
    server.serve_stdio().await?;
    Ok(())
}

/// Resolve the full config (embedding engines + actor namespace) from file or env.
///
/// Precedence for actor/namespace (highest to lowest):
/// 1. CLI `--actor` / `--namespace` (cli_namespace_explicit=true skips config tier)
/// 2. Config file `[actor] id` (applied here when !cli_namespace_explicit)
/// 3. Default "local" from RuntimeConfig
///
/// Precedence for embedding engines:
/// 1. Config file `[[engines]]`
/// 2. Env vars `KHIVE_EMBEDDING_MODEL` + `KHIVE_ADDITIONAL_EMBEDDING_MODELS`
fn resolve_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
    cli_namespace_explicit: bool,
) -> anyhow::Result<RuntimeConfig> {
    match KhiveConfig::load_with_home_fallback(config_path)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    {
        Some(khive_cfg) => {
            // Config file present — check if env vars are also set and warn.
            let env_primary = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
            let env_additional = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS").ok();
            if env_primary.is_some() || env_additional.is_some() {
                tracing::warn!(
                    "khive config file is present; KHIVE_EMBEDDING_MODEL and \
                     KHIVE_ADDITIONAL_EMBEDDING_MODELS env vars are ignored"
                );
            }

            // When the caller supplied --actor or --namespace, the CLI value wins
            // over [actor] id in the config file. Nullify config actor so
            // runtime_config_from_khive_config does not overwrite the base namespace.
            let effective_cfg = if cli_namespace_explicit {
                let mut c = khive_cfg;
                c.actor.id = None;
                c
            } else {
                khive_cfg
            };

            Ok(runtime_config_from_khive_config(&effective_cfg, base))
        }
        None => {
            // No config file — fall back to env-var embedding path.
            let env_cfg = config_from_env();
            if env_cfg.engines.is_empty() {
                Ok(base)
            } else {
                Ok(runtime_config_from_khive_config(&env_cfg, base))
            }
        }
    }
}

/// Resolve only the actor namespace from a config file (no-embed path).
///
/// Used when `--no-embed` zeroed out embedding; we still want config-file
/// `[actor] id` to apply if no CLI actor was given.
fn resolve_actor_from_config(
    config_path: Option<&std::path::Path>,
    base: RuntimeConfig,
    cli_namespace_explicit: bool,
) -> anyhow::Result<RuntimeConfig> {
    if cli_namespace_explicit {
        return Ok(base);
    }
    match KhiveConfig::load_with_home_fallback(config_path)
        .map_err(|e| anyhow::anyhow!("config error: {e}"))?
    {
        Some(khive_cfg) => {
            // KhiveConfig::validate() already ran inside load_with_home_fallback,
            // so actor.id is guaranteed valid here. runtime_config_from_khive_config
            // applies the actor, but we zero out embedding fields after the fact to
            // preserve the --no-embed contract.
            let resolved = runtime_config_from_khive_config(&khive_cfg, base);
            Ok(RuntimeConfig {
                embedding_model: None,
                additional_embedding_models: vec![],
                ..resolved
            })
        }
        None => Ok(base),
    }
}
