//! CLI argument definition and namespace resolution for `khive-mcp`.
//!
//! Extracted from `main.rs` so integration tests can exercise the real `Args`
//! parser with `Args::try_parse_from` without spawning the binary.

use std::path::PathBuf;

use clap::Parser;
use khive_runtime::Namespace;

/// Parsed command-line arguments for the `khive-mcp` binary.
#[derive(Parser, Debug)]
#[command(
    name = "khive-mcp",
    version,
    about = "khive MCP server (stdio) — the only user-facing Rust binary"
)]
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

    /// Log level for stderr output (stdout is reserved for the MCP protocol).
    #[arg(long, env = "KHIVE_LOG", default_value = "warn")]
    pub log: String,

    /// Pack to load into the verb registry. Repeat for multiple
    /// (e.g. `--pack kg --pack gtd`). Falls back to `KHIVE_PACKS` env
    /// (comma- or whitespace-separated) or `["kg"]` if neither is set.
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

    /// Run as a persistent daemon over a Unix socket instead of stdio.
    ///
    /// The daemon owns the warm pack registry (ANN indexes) and serves request
    /// frames from thin stdio clients that auto-spawn it. Bound to
    /// `~/.khive/khived.sock`.
    #[arg(long)]
    pub daemon: bool,
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
