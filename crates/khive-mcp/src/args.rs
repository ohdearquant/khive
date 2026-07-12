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
    /// Precedence (highest to lowest — ADR-096 Fork 2, ratified 2026-07-05):
    ///   1. --actor (this flag) / --namespace / KHIVE_NAMESPACE (legacy alias)
    ///   2. \[actor\] id in the cwd/project-anchored config
    ///      (`.khive/config.toml`, resolved independently of `--db`)
    ///   3. KHIVE_ACTOR env var (attribution-only fallback — see below)
    ///   4. Default: "local" (anonymous)
    ///
    /// KHIVE_ACTOR is intentionally NOT bound as a clap `env` source on this
    /// field: doing so would make a bare shell-level `KHIVE_ACTOR` collapse
    /// into tier 1 and beat tier 2, which inverts the ratified precedence.
    /// It is read directly as the tier-3 fallback in
    /// `RuntimeConfig::default()` / `resolve_runtime_config` instead, and it
    /// sets `actor_id` only — never `default_namespace` (identity is not
    /// namespace, ADR-007 Rule 0).
    #[arg(long)]
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
    /// production set: `kg,gtd,memory,brain,comm,schedule,knowledge,session,git,code,workspace`.
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

    /// Brain profile ID for feedback routing and recall-time score boosting.
    ///
    /// When set, `memory.feedback` and `knowledge.feedback` credit this profile.
    /// Takes the highest precedence in the resolution chain.
    ///
    /// Precedence (highest to lowest):
    ///   1. --brain-profile (this flag)
    ///   2. \[runtime\] brain_profile in project khive.toml / global ~/.khive/config.toml
    ///   3. KHIVE_BRAIN_PROFILE env var
    ///   4. Namespace-bound profile (resolved at feedback time via brain.resolve)
    ///   5. Pack-local global tuning prior
    ///
    /// Note: KHIVE_BRAIN_PROFILE is NOT bound here so that the env var resolves
    /// AFTER the config-file tier (serve.rs reads it explicitly after TOML).
    #[arg(long)]
    pub brain_profile: Option<String>,

    /// Internal marker: this process is a resumed generation of a stdio
    /// bridge that just re-exec'd itself in place after detecting a stale
    /// daemon-protocol mismatch (issue #714). Never set by a normal client
    /// launch — the bridge appends it to its own preserved argv immediately
    /// before `exec()`ing the freshest on-disk binary, so the value travels
    /// with the process image across the exec by construction.
    ///
    /// When present, `KhiveMcpServer::serve_stdio` skips the MCP initialize
    /// handshake (`serve_directly`) instead of waiting for one, since the
    /// connected peer already completed a real handshake with the prior
    /// generation over this same, uninterrupted stdio pipe. It also gates the
    /// re-exec loop-breaker: a resumed generation that itself observes a
    /// protocol mismatch again takes the drain-and-exit fallback rather than
    /// exec'ing a second time (`crate::daemon`, guard rail #714 §2.2).
    #[arg(long, hide = true)]
    pub resumed_generation: Option<u32>,
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
