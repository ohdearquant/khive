//! Build the runtime + server from CLI args and serve over the selected transport.
//!
//! This is the bootstrap that the `kkernel mcp` subcommand drives. Logging is
//! initialized by the binary, not here.

use std::path::PathBuf;

use khive_runtime::{
    config_from_env, runtime_config_from_khive_config, KhiveConfig, KhiveRuntime, RuntimeConfig,
};

use crate::args::{resolve_cli_namespace, Args};
use crate::server::KhiveMcpServer;
use crate::transport::{ServeOptions, TransportRegistry};

/// Build a server from `args`, then serve it over `--daemon` or the named transport.
pub async fn run(args: Args, registry: &TransportRegistry) -> anyhow::Result<()> {
    let server = build_server(&args)?;

    #[cfg(unix)]
    if args.daemon {
        khive_runtime::daemon::run_daemon(server).await?;
        return Ok(());
    }
    #[cfg(not(unix))]
    if args.daemon {
        anyhow::bail!(
            "--daemon mode requires Unix (macOS/Linux). On Windows, use the stdio transport."
        );
    }

    let transport_name = args.transport.as_deref().unwrap_or("stdio");
    let transport = registry.get(transport_name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown transport {transport_name:?}; registered: {}",
            registry.names().join(", ")
        )
    })?;
    let opts = ServeOptions {
        bind: args.bind.clone(),
    };
    transport.serve(server, &opts).await
}

/// Build a fully-configured server from parsed args (without serving).
pub fn build_server(args: &Args) -> anyhow::Result<KhiveMcpServer> {
    let db_path = match args.db.as_deref() {
        Some(":memory:") => None,
        Some(path) => Some(PathBuf::from(path)),
        None => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            Some(PathBuf::from(format!("{home}/.khive/khive.db")))
        }
    };

    let (cli_namespace_explicit, cli_namespace) =
        resolve_cli_namespace(args).map_err(|e| anyhow::anyhow!("{e}"))?;

    let packs = if args.pack.is_empty() {
        RuntimeConfig::default().packs
    } else {
        args.pack.clone()
    };

    let base_config = RuntimeConfig {
        db_path,
        default_namespace: cli_namespace,
        packs,
        ..RuntimeConfig::default()
    };

    let config = if args.no_embed {
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
    KhiveMcpServer::new(runtime).map_err(|e| anyhow::anyhow!("{e}"))
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
            let env_primary = std::env::var("KHIVE_EMBEDDING_MODEL").ok();
            let env_additional = std::env::var("KHIVE_ADDITIONAL_EMBEDDING_MODELS").ok();
            if env_primary.is_some() || env_additional.is_some() {
                tracing::warn!(
                    "khive config file is present; KHIVE_EMBEDDING_MODEL and \
                     KHIVE_ADDITIONAL_EMBEDDING_MODELS env vars are ignored"
                );
            }

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
