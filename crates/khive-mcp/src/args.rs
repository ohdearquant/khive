//! Serve-time argument definition and namespace resolution for `kkernel mcp`.
//!
//! These are the args the `kkernel mcp` subcommand flattens. Logging is owned by
//! the binary's global `--log`, so it is intentionally absent here.

use std::path::PathBuf;

use clap::Parser;
use khive_runtime::Namespace;

/// Parsed serve-time arguments for the `kkernel mcp` subcommand.
#[derive(Parser, Debug)]
pub struct Args {
    /// Path to the khive database. Use \":memory:\" for an ephemeral in-memory database.
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,

    /// Default actor (namespace) for all operations that do not specify one.
    ///
    /// Overrides any `[actor] id = ...` value in the config file. In OSS mode
    /// this is advisory — no token enforcement is applied. Cloud deployments
    /// derive namespace from the authenticated session token instead.
    ///
    /// Precedence (highest to lowest):
    ///   1. --actor (this flag)
    ///   2. --namespace / KHIVE_NAMESPACE (legacy alias)
    ///   3. [actor] id in config file (--config / KHIVE_CONFIG / khive.toml / ~/.khive/config.toml)
    ///   4. Default: "local"
    #[arg(long, env = "KHIVE_ACTOR")]
    pub actor: Option<String>,

    /// Default namespace for operations that do not specify one (legacy alias for --actor).
    ///
    /// Use --actor for new deployments. When both --actor and --namespace are
    /// supplied, --actor wins.  When neither is supplied the value is `None`
    /// and the default `"local"` is applied after config-file resolution.
    #[arg(long, env = "KHIVE_NAMESPACE")]
    pub namespace: Option<String>,

    /// Disable local embedding model (skips vector indexing on create/update).
    #[arg(long, env = "KHIVE_NO_EMBED")]
    pub no_embed: bool,

    /// Pack to load into the verb registry. Repeat for multiple
    /// (e.g. `--pack kg --pack gtd`). When unset, falls back to `KHIVE_PACKS`
    /// (comma- or whitespace-separated), and if that is also unset to the full
    /// production set: `kg,gtd,memory,brain,comm,schedule,knowledge`.
    #[arg(long = "pack")]
    pub pack: Vec<String>,

    /// Path to a khive TOML config file.
    ///
    /// When provided, embedding engine and actor configuration are loaded from
    /// this file. Overrides env vars `KHIVE_EMBEDDING_MODEL` and
    /// `KHIVE_ADDITIONAL_EMBEDDING_MODELS`. The `[actor] id` in this file
    /// sets the default namespace (overridden by --actor).
    ///
    /// Default search order when this flag is absent:
    ///   1. ./khive.toml
    ///   2. ./.khive/config.toml
    ///   3. ~/.khive/config.toml
    #[arg(long = "config", env = "KHIVE_CONFIG")]
    pub config: Option<PathBuf>,

    /// Run as a persistent daemon over a Unix socket instead of a foreground transport.
    ///
    /// The daemon owns the warm pack registry (ANN indexes) and serves request
    /// frames from thin stdio clients that auto-spawn it. Bound to
    /// `~/.khive/khived.sock`. Takes precedence over `--transport`.
    #[arg(long)]
    pub daemon: bool,

    /// Foreground serving transport (registry name). Defaults to `stdio`.
    ///
    /// Additional transports (e.g. Streamable HTTP) can be registered before
    /// serving; an unknown name errors with the registered set.
    #[arg(long)]
    pub transport: Option<String>,

    /// Bind address for network transports (e.g. `0.0.0.0:8080`). Ignored by stdio.
    #[arg(long)]
    pub bind: Option<String>,
}

/// Resolve CLI namespace from `Args`. Returns `(explicit, namespace)`; errors on invalid namespace string.
pub fn resolve_cli_namespace(args: &Args) -> Result<(bool, Namespace), String> {
    let explicit = args.actor.is_some() || args.namespace.is_some();
    let raw = args
        .actor
        .as_deref()
        .or(args.namespace.as_deref())
        .unwrap_or("local");
    let ns = Namespace::parse(raw).map_err(|e| format!("invalid namespace {raw:?}: {e}"))?;
    Ok((explicit, ns))
}
