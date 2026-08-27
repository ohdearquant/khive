//! `kkernel` CLI — khive admin/management command surface (the kernel/MCP
//! split keeps admin and infrastructure operations out of the MCP surface).
//!
//! Lives in the library (rather than `main.rs`) so downstream distributions
//! can embed the full CLI in their own binary with additional packs linked
//! in: `fn main() { kkernel::cli::cli_main() }` plus a force-link `use` per
//! extra pack crate (ADR-027 self-registration).
//!
//! See `crates/kkernel/docs/usage.md` for the full subcommand reference.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use crate::{
    code_audit, code_ingest,
    coordinator::{BackendRegistry, SubstrateCoordinator, SubstrateCoordinatorService},
    engine, exec, git_ingest, kg, pack_introspect, reindex, repo, sync, vector,
};
use khive_runtime::{
    runtime_config_from_khive_config, BackendId, BackendKind, KhiveConfig, KhiveRuntime,
    RuntimeConfig,
};

#[derive(Parser, Debug)]
#[command(
    name = "kkernel",
    version,
    long_version = khive_runtime::BUILD_VERSION,
    about = "khive kernel — admin/management Rust binary"
)]
struct Args {
    /// Log level for stderr output. JSON results go to stdout regardless.
    #[arg(long, env = "KHIVE_LOG", default_value = "warn", global = true)]
    log: String,

    /// Quick-shot: run a verb DSL expression, shorthand for `kkernel exec <OPS>`.
    ///
    /// `kkernel -e '<ops>'` is equivalent to `kkernel exec '<ops>'` with every
    /// other `exec` flag at its default (db/namespace resolution, presentation,
    /// output format, ...). For `exec`'s other flags (`--ops-file`, `--db`,
    /// `--namespace`, `--presentation`, ...), use the full `kkernel exec`
    /// subcommand instead. Mutually exclusive with a subcommand (clap subcommand
    /// fields are not directly addressable via `conflicts_with`, so this is
    /// enforced explicitly in `main()`, right after `Args::parse()`, with the
    /// same `clap::Command::error(...).exit()` mechanism clap itself uses).
    #[arg(short = 'e', long = "exec", value_name = "OPS")]
    exec: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Build a working SQLite DB from .khive/kg/*.ndjson sources (issue #174).
    Sync(SyncArgs),

    /// Introspect registered packs.
    #[command(subcommand)]
    Pack(PackCommand),

    /// KG validation, init, and hook management.
    #[command(subcommand)]
    Kg(kg::KgCommand),

    /// Build and export offline repository showcase bundles (ADR-147).
    #[command(subcommand)]
    Repo(repo::RepoCommand),

    /// Schema migration lifecycle: migrate and check.
    #[command(subcommand)]
    Db(DbCommand),

    /// Embedding model lifecycle: list, status, migrate, drift-check.
    #[command(subcommand)]
    Engine(engine::EngineCommand),

    /// Vector store capabilities and orphan sweep.
    #[command(subcommand)]
    Vector(vector::VectorCommand),

    /// Re-embed entities, notes, and the knowledge corpus, fanning out across
    /// every configured embedding engine (resolved like `kkernel mcp`).
    Reindex(reindex::ReindexArgs),

    /// Execute a verb DSL expression (same syntax as MCP `request` tool).
    Exec(exec::ExecArgs),

    /// Serve the MCP `request` surface (stdio by default; `--daemon` for the
    /// warm Unix-socket server; `--transport` selects a registered transport).
    Mcp(khive_mcp::args::Args),

    /// Serve the dedicated events daemon (ADR-170): the resident writer of
    /// the events database, receiving observational events over its own Unix
    /// socket so telemetry never queues on the domain store's writer lane.
    /// Normally spawned and supervised by `kkernel mcp --daemon`, not run by
    /// hand.
    EventsDaemon(EventsDaemonArgs),

    /// Inspect registered backends.
    #[command(subcommand)]
    Backend(BackendCommand),

    /// Read-only derived-report pass over a dedicated code-map database
    /// (ADR-Q1/Q2 phase 1). Never writes to any graph.
    CodeAudit(code_audit::CodeAuditArgs),

    /// One-shot batch ingest of commit/issue/pull_request provenance notes
    /// from a local git repository (ADR-088).
    GitIngest(git_ingest::GitIngestArgs),

    /// Validate and ingest a `findings.json` audit sweep into the graph as
    /// `finding` notes (ADR-085 Amendment 3).
    CodeIngest(code_ingest::CodeIngestArgs),
}

/// Arguments for the dedicated events daemon (ADR-170).
#[derive(clap::Parser, Debug)]
struct EventsDaemonArgs {
    /// Events database file. Defaults to `<main-file-name>.events.db` beside the
    /// resolved main database (`--db`/`KHIVE_DB` resolution applies to the
    /// MAIN database; this flag names the events file itself).
    #[arg(long)]
    db: Option<PathBuf>,

    /// Unix socket path to bind. Defaults to the events database path with a
    /// `.sock` extension, beside that database.
    #[arg(long)]
    socket: Option<PathBuf>,
}

/// Database schema lifecycle subcommands.
#[derive(Subcommand, Debug)]
enum DbCommand {
    /// Apply any pending schema migrations to the configured database.
    Migrate(DbMigrateArgs),

    /// Report per-backend schema state without applying changes.
    Check(DbCheckArgs),
}

#[derive(clap::Parser, Debug)]
struct DbMigrateArgs {
    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long, env = "KHIVE_DB")]
    db: Option<String>,

    /// Explicit khive config path (otherwise use normal discovery).
    #[arg(long, env = "KHIVE_CONFIG")]
    config: Option<PathBuf>,

    /// Target a specific backend by name.
    #[arg(long)]
    backend: Option<String>,

    /// Show what would be applied without executing migrations.
    #[arg(long)]
    dry_run: bool,

    /// Exit 0 if current, nonzero if any migration is pending (implies --dry-run).
    #[arg(long)]
    check: bool,

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    human: bool,
}

#[derive(clap::Parser, Debug)]
struct DbCheckArgs {
    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long, env = "KHIVE_DB")]
    db: Option<String>,

    /// Explicit khive config path (otherwise use normal discovery).
    #[arg(long, env = "KHIVE_CONFIG")]
    config: Option<PathBuf>,

    /// Target a specific backend by name.
    #[arg(long)]
    backend: Option<String>,

    /// Exit nonzero if any backend is behind the current schema version.
    #[arg(long)]
    strict: bool,

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    human: bool,
}

#[derive(Parser, Debug)]
struct SyncArgs {
    /// Repository root containing .khive/kg/{entities,edges}.ndjson.
    #[arg(long, default_value = ".")]
    repo: PathBuf,

    /// Output SQLite database path. Replaced atomically via tmp+rename.
    #[arg(long)]
    db: PathBuf,

    /// Namespace for imported records.
    #[arg(long, default_value = "local")]
    namespace: String,
}

#[derive(Subcommand, Debug)]
enum PackCommand {
    /// List all registered packs with their verb / note kind / entity kind surface.
    List {
        /// Print a human-readable table instead of JSON.
        #[arg(long)]
        human: bool,
    },

    /// Print the full handler surface for one pack.
    Handler {
        /// Pack name (e.g. `kg`, `gtd`, `memory`).
        name: String,

        /// Print a human-readable layout instead of JSON.
        #[arg(long)]
        human: bool,
    },
}

/// Backend admin commands.
///
/// In the full multi-backend deployment, `kkernel backend list` reads `khive.toml`
/// and enumerates all configured `[[backends]]` entries. In the current v1 implementation,
/// it lists the single default backend constructed from `RuntimeConfig::default()`.
#[derive(Subcommand, Debug)]
enum BackendCommand {
    /// List all registered backends.
    List {
        /// Print a human-readable table instead of JSON.
        #[arg(long)]
        human: bool,
    },

    /// Print information about a specific backend.
    Info {
        /// Backend name (e.g. `main`, `lore`, `archive`).
        name: String,

        /// Print human-readable output instead of JSON.
        #[arg(long)]
        human: bool,
    },
}

/// Load `~/.khive/.env` into the process environment if present.
///
/// khive reads all configuration from process env (`std::env::var`), so this
/// makes `~/.khive/.env` the canonical config home — credentials set there
/// reach the daemon however it is spawned. Real environment variables win over
/// the file (dotenvy does not override what is already set), and a missing file
/// is not an error.
fn load_khive_dotenv() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let path = std::path::Path::new(&home).join(".khive/.env");
    match dotenvy::from_path(&path) {
        Ok(()) => {}
        Err(e) if e.not_found() => {}
        Err(e) => eprintln!("warning: failed to load {}: {e}", path.display()),
    }
}

/// Entry point for the `kkernel` CLI. Public so downstream binaries can
/// embed the full command surface with additional pack crates force-linked.
#[tokio::main]
pub async fn cli_main() -> Result<()> {
    load_khive_dotenv();
    let args = Args::parse();
    init_tracing(&args.log);

    // `-e/--exec` is the quick-shot equivalent of `exec <OPS>` — route it
    // through the exact same clap parsing `exec` itself uses (`ExecArgs::parse_from`)
    // so behavior (env bindings, defaults) is byte-identical to typing out the
    // subcommand. `-e` and a subcommand are mutually exclusive; a `#[command(subcommand)]`
    // field cannot be named in a plain `#[arg(conflicts_with = ...)]` (clap rejects that
    // at startup with a debug_assert — verified empirically), so the conflict is enforced
    // here instead, using the same `clap::Command::error(...).exit()` mechanism clap's own
    // built-in conflict detection uses (matching exit code + message style).
    let command = resolve_command(args.exec, args.command);

    match command {
        Command::Sync(s) => cmd_sync(s).await,
        Command::Pack(p) => cmd_pack(p),
        Command::Kg(k) => kg::run_kg(k).await,
        Command::Repo(r) => repo::run_repo(r).await,
        Command::Db(d) => cmd_db(d).await,
        Command::Engine(e) => engine::run_engine(e).await,
        Command::Vector(v) => vector::run_vector(v),
        Command::Reindex(r) => reindex::run_reindex(r).await,
        Command::Exec(e) => {
            let result = exec::run_exec(e).await;
            if let Err(error) = &result {
                if let Some(envelope) = khive_mcp::serve::db_override_refusal_envelope(error) {
                    println!(
                        "{}",
                        serde_json::to_string(&envelope)
                            .expect("database override refusal envelope must serialize")
                    );
                }
            }
            result
        }
        #[cfg(unix)]
        Command::EventsDaemon(a) => {
            let db = match a.db {
                Some(db) => db,
                None => {
                    let main_db = khive_runtime::resolve_db_anchor(None).ok_or_else(|| {
                        anyhow::anyhow!(
                            "events-daemon: no main database resolvable to anchor the events \
                             database; pass --db explicitly"
                        )
                    })?;
                    khive_runtime::events_split::events_db_path_beside(&main_db)
                }
            };
            let socket = a
                .socket
                .unwrap_or_else(|| khive_runtime::events_split::events_socket_path_beside(&db));
            khive_runtime::events_split::run_events_daemon(&db, &socket).await
        }
        #[cfg(not(unix))]
        Command::EventsDaemon(_) => {
            anyhow::bail!("the events daemon requires a Unix platform (Unix-socket transport)")
        }
        Command::Mcp(a) => {
            let transport_registry = khive_mcp::transport::TransportRegistry::with_builtins();

            // Check if multi-backend is configured (ADR-028 / ADR-029 Phase 2).
            //
            // Resolve the tier-3 discovery anchor with the SAME
            // `config_discovery_db_anchor` semantics `resolve_runtime_config`
            // below uses (explicit `--db`/`KHIVE_DB` -> that path; unset ->
            // `None`, falling through to cwd-anchored discovery) so this early
            // multi-backend classification sees the identical config file the
            // per-request `config_id` path resolves further down (#689: using
            // `resolve_db_anchor`'s materialized `$HOME/.khive/khive.db`
            // default here anchored classification to the home directory
            // instead of the project, silently skipping a project-local
            // `.khive/config.toml`).
            let db_path_hint = khive_mcp::serve::config_discovery_db_anchor(a.db.as_deref());
            // An explicit `--config`/`KHIVE_CONFIG` that fails to load — a
            // malformed file OR a missing one — must fail loud: silently
            // defaulting would boot against a config the operator did not
            // select (ADR-035). The loader itself enforces the explicit tier
            // (`load_with_home_fallback_and_source` returns
            // `ConfigError::ExplicitConfigMissing` for a missing explicit
            // path), so every entry point inherits the refusal; the
            // automatic-discovery tiers keep the historical tolerant
            // default: the CLI-level topology check is advisory, and a
            // malformed discovered file is still reported by the builder's
            // own load further down.
            let loaded_config = match KhiveConfig::load_with_home_fallback_and_source(
                a.config.as_deref(),
                db_path_hint.as_deref(),
            ) {
                Ok(loaded) => loaded,
                Err(load_error) => {
                    if a.config.is_some() {
                        return Err(load_error)
                            .context("failed to load the explicitly selected config file");
                    }
                    None
                }
            };
            let config_source = loaded_config.as_ref().map(|(_, source)| source.as_path());
            let khive_cfg = loaded_config
                .as_ref()
                .map(|(config, _)| config.clone())
                .unwrap_or_default();

            if !khive_cfg.backends.is_empty() {
                khive_mcp::serve::reject_conflicting_db_override_with_source(
                    a.db.as_deref(),
                    &khive_cfg.backends,
                    config_source,
                )?;
            }

            if khive_cfg.backends.len() <= 1 {
                // Single-backend: zero-change path — no coordinator.
                khive_mcp::serve::run(a, &transport_registry).await
            } else {
                // Multi-backend: build registry, attach the SubstrateCoordinator,
                // and finish server assembly through the shared #603 constructor
                // (`build_multi_backend_server_with_coordinator`) — this branch
                // contains no server-assembly logic of its own beyond building
                // the coordinator's BackendRegistry and attaching it.
                let (cli_ns_explicit, cli_ns) = khive_mcp::args::resolve_cli_namespace(&a)
                    .map_err(|e| anyhow::anyhow!("{e}"))?;

                let (base_cfg, db_anchor) =
                    khive_mcp::serve::resolve_runtime_config_with_db_anchor(
                        khive_mcp::serve::RuntimeConfigInputs {
                            db: a.db.as_deref(),
                            config: a.config.as_deref(),
                            namespace: cli_ns,
                            namespace_explicit: cli_ns_explicit,
                            actor_explicit: cli_ns_explicit,
                            no_embed: a.no_embed,
                            packs: if a.pack.is_empty() {
                                None
                            } else {
                                Some(a.pack.clone())
                            },
                            brain_profile: a.brain_profile.clone(),
                        },
                    )?;
                // ADR-170: this arm is a resident daemon host when `--daemon`
                // is set — it supervises an events daemon at the derived
                // socket (`start_daemon_components_if_daemon`), so upgrade
                // the resolved event plane from direct mode to forwarding.
                let base_cfg = {
                    let mut base_cfg = base_cfg;
                    if a.daemon {
                        khive_mcp::serve::enable_events_forwarding_for_daemon(&mut base_cfg);
                    }
                    base_cfg
                };

                // #667: acquire the boot/recovery lock before building the
                // coordinator server — that construction runs migrations and
                // applies pack schema plans (FTS DDL included) — and hold it
                // through `serve_server`'s daemon bind+pid-write, so a second
                // concurrently-booting process cannot run schema DDL against
                // the same database file at the same time. In daemon mode,
                // failing to acquire the lock here must abort before that
                // unguarded construction runs, rather than silently
                // proceeding with `boot_guard = None`.
                #[cfg(unix)]
                let boot_guard = if a.daemon {
                    Some(khive_runtime::daemon::acquire_daemon_boot_guard()?)
                } else {
                    khive_runtime::daemon::acquire_recovery_lock()
                };
                #[cfg(not(unix))]
                let boot_guard: Option<std::fs::File> = None;

                let (server, schedule_rt) =
                    build_multi_backend_server_with_coordinator_and_db_anchor(
                        base_cfg,
                        &khive_cfg,
                        a.db.as_deref(),
                        db_anchor.as_deref(),
                    )
                    .await?;

                khive_mcp::serve::serve_server(
                    server,
                    &a,
                    &transport_registry,
                    boot_guard,
                    schedule_rt,
                )
                .await
            }
        }
        Command::Backend(b) => cmd_backend(b),
        Command::CodeAudit(a) => code_audit::run_code_audit(a).await,
        Command::GitIngest(a) => git_ingest::run_git_ingest(a).await,
        Command::CodeIngest(a) => code_ingest::run_code_ingest(a).await,
    }
}

/// Why `-e`/subcommand resolution failed — see [`resolve_command_result`].
#[derive(Debug, PartialEq, Eq)]
enum ResolveCommandError {
    /// Neither `-e <OPS>` nor a subcommand was given.
    Missing,
    /// Both `-e <OPS>` and a subcommand were given.
    Conflict,
}

/// Pure resolution of the effective `Command` from the two mutually exclusive
/// top-level entry points (`-e/--exec` vs. a subcommand); split out from
/// [`resolve_command`] so all four cases are unit-testable without triggering
/// clap's process-exiting `.error(...).exit()` path. `-e <OPS>` reparses through
/// `ExecArgs::parse_from(["exec", "--", &ops])` (byte-identical to `exec -- <OPS>`).
/// See `crates/kkernel/docs/coordinator.md` for why the exec/subcommand conflict
/// can't be declared on the field itself via clap's `conflicts_with`.
fn resolve_command_result(
    exec: Option<String>,
    command: Option<Command>,
) -> Result<Command, ResolveCommandError> {
    match (exec, command) {
        (Some(ops), None) => Ok(Command::Exec(exec::ExecArgs::parse_from([
            "exec", "--", &ops,
        ]))),
        (None, Some(cmd)) => Ok(cmd),
        (None, None) => Err(ResolveCommandError::Missing),
        (Some(_), Some(_)) => Err(ResolveCommandError::Conflict),
    }
}

/// `main()`'s entry point into [`resolve_command_result`]: same resolution,
/// but turns a `Missing`/`Conflict` error into a clap-style CLI error (matching
/// exit code 2 and clap's own error-printing style) instead of returning it.
fn resolve_command(exec: Option<String>, command: Option<Command>) -> Command {
    use clap::{error::ErrorKind, CommandFactory};
    match resolve_command_result(exec, command) {
        Ok(cmd) => cmd,
        Err(ResolveCommandError::Missing) => Args::command()
            .error(
                ErrorKind::MissingRequiredArgument,
                "either provide -e/--exec <OPS> or a subcommand",
            )
            .exit(),
        Err(ResolveCommandError::Conflict) => Args::command()
            .error(
                ErrorKind::ArgumentConflict,
                "the argument '-e/--exec <OPS>' cannot be used with a subcommand",
            )
            .exit(),
    }
}

/// Build the coordinator-attached multi-backend server for `kkernel mcp` (the
/// `Command::Mcp` branch, when `[[backends]]` declares more than one backend).
/// See `crates/kkernel/docs/coordinator.md#kkernel-mainrs--coordinator-attached-boot-path`
/// for why this is the one place that assembles the coordinator inputs.
#[cfg(test)]
async fn build_multi_backend_server_with_coordinator(
    base_cfg: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
) -> Result<(khive_mcp::server::KhiveMcpServer, Option<KhiveRuntime>)> {
    let db_anchor = if cli_db_override == Some(":memory:") {
        None
    } else {
        base_cfg.db_path.clone()
    };
    build_multi_backend_server_with_coordinator_and_db_anchor(
        base_cfg,
        khive_cfg,
        cli_db_override,
        db_anchor.as_deref(),
    )
    .await
}

async fn build_multi_backend_server_with_coordinator_and_db_anchor(
    base_cfg: RuntimeConfig,
    khive_cfg: &KhiveConfig,
    cli_db_override: Option<&str>,
    db_anchor: Option<&std::path::Path>,
) -> Result<(khive_mcp::server::KhiveMcpServer, Option<KhiveRuntime>)> {
    let multi = khive_mcp::serve::build_registry_for_multi_backend_with_db_anchor(
        base_cfg,
        khive_cfg,
        cli_db_override,
        db_anchor,
    )
    .await?;

    let schedule_rt = multi
        .per_pack_runtimes
        .get("schedule")
        .map(|rt| (**rt).clone());

    // Build BackendRegistry: one entry per unique backend (deduplicated
    // by backend_name so packs sharing a backend share one runtime).
    let mut backend_reg = BackendRegistry::new();
    for (pack_name, rt) in &multi.per_pack_runtimes {
        let backend_name = khive_cfg
            .packs
            .get(pack_name.as_str())
            .map(|pc| pc.backend.as_str())
            .unwrap_or(BackendId::MAIN);
        let backend_id = BackendId::new(backend_name);
        // `BackendRegistry::register` is idempotent by backend_id —
        // the second registration for the same id is a no-op.
        backend_reg.register(backend_id, Arc::clone(rt));
    }

    let coord = SubstrateCoordinatorService::new(SubstrateCoordinator::new(backend_reg));

    let server = khive_mcp::serve::build_server_from_multi_backend_registry(
        multi,
        khive_cfg,
        Some(Arc::new(coord) as Arc<dyn khive_mcp::coordinator::CoordinatorService>),
    );
    Ok((server, schedule_rt))
}

async fn cmd_db(cmd: DbCommand) -> Result<()> {
    match cmd {
        DbCommand::Migrate(args) => cmd_db_migrate(args).await,
        DbCommand::Check(args) => cmd_db_check(args).await,
    }
}

struct DbCommandContext {
    base_config: RuntimeConfig,
    khive_config: KhiveConfig,
    cli_db_override: Option<String>,
}

fn resolve_db_command_context(
    db: Option<&str>,
    config: Option<&std::path::Path>,
) -> Result<DbCommandContext> {
    let discovery_anchor = khive_mcp::serve::config_discovery_db_anchor(db);
    let loaded =
        KhiveConfig::load_with_home_fallback_and_source(config, discovery_anchor.as_deref())
            .context("load database-command khive config")?;
    let config_source = loaded.as_ref().map(|(_, source)| source.as_path());
    let khive_config = loaded
        .as_ref()
        .map(|(config, _)| config.clone())
        .unwrap_or_default();
    khive_mcp::serve::reject_conflicting_db_override_with_source(
        db,
        &khive_config.backends,
        config_source,
    )?;

    let base_config = RuntimeConfig {
        db_path: khive_runtime::resolve_db_anchor(db),
        ..RuntimeConfig::default()
    };
    let mut base_config = runtime_config_from_khive_config(&khive_config, base_config);
    // Schema administration never needs to instantiate an embedding model or
    // register packs. Blob hydration remains configured because verified V20
    // moodboard evidence may be part of the cutover.
    base_config.embedding_model = None;
    base_config.additional_embedding_models.clear();
    base_config.packs.clear();

    Ok(DbCommandContext {
        base_config,
        khive_config,
        cli_db_override: db.map(str::to_owned),
    })
}

async fn cmd_db_migrate(args: DbMigrateArgs) -> Result<()> {
    if args.dry_run || args.check {
        // For dry-run / --check, query the current schema version without writing.
        return cmd_db_check(DbCheckArgs {
            db: args.db,
            config: args.config,
            backend: args.backend,
            strict: args.check,
            human: args.human,
        })
        .await;
    }

    let context = resolve_db_command_context(args.db.as_deref(), args.config.as_deref())?;

    // V21 is application-assisted: after the ordinary V1-V20 schema prefix,
    // the host must install bounded blob hydration, authenticate any legacy
    // moodboard bundle/network evidence, and only then finalize attachment-only
    // liveness. Reuse the same async choke point as MCP/exec boot so the admin
    // command cannot strand an incomplete database or bypass the GC fence.
    let statuses = khive_mcp::serve::migrate_configured_storage_topology(
        context.base_config,
        &context.khive_config,
        context.cli_db_override.as_deref(),
        args.backend.as_deref(),
    )
    .await
    .context("migrate configured storage topology through the attachment cutover")?;
    let latest = khive_db::MIGRATIONS
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0);
    let current = statuses
        .iter()
        .all(|status| status.applied_version == latest);

    if args.human {
        for status in &statuses {
            let role = if status.prerequisite {
                " prerequisite"
            } else {
                ""
            };
            println!(
                "{}: V{} of V{} (current{role})",
                status.backend, status.applied_version, latest
            );
        }
    } else {
        let backends = statuses
            .iter()
            .map(|status| {
                serde_json::json!({
                    "backend": status.backend,
                    "applied_version": status.applied_version,
                    "latest_version": latest,
                    "current": status.applied_version == latest,
                    "prerequisite": status.prerequisite,
                })
            })
            .collect::<Vec<_>>();
        let json = serde_json::json!({
            "latest_version": latest,
            "current": current,
            "backends": backends,
        });
        println!("{}", serde_json::to_string(&json).expect("serialize"));
    }
    Ok(())
}

async fn cmd_db_check(args: DbCheckArgs) -> Result<()> {
    let context = resolve_db_command_context(args.db.as_deref(), args.config.as_deref())?;
    let latest = khive_db::MIGRATIONS
        .last()
        .map(|migration| migration.version)
        .unwrap_or(0);
    let force_memory = context.cli_db_override.as_deref() == Some(":memory:");

    let mut targets = Vec::new();
    if context.khive_config.backends.is_empty() {
        khive_mcp::serve::configured_storage_check_targets(
            &context.khive_config,
            context.cli_db_override.as_deref(),
            args.backend.as_deref(),
        )?;
        targets.push((BackendId::MAIN.to_string(), context.base_config.db_path));
    } else {
        let planned_names = khive_mcp::serve::configured_storage_check_targets(
            &context.khive_config,
            context.cli_db_override.as_deref(),
            args.backend.as_deref(),
        )?;
        for backend_name in planned_names {
            let backend = context
                .khive_config
                .backends
                .iter()
                .find(|backend| backend.name == backend_name)
                .expect("the shared topology planner returned a configured backend");
            let path = if force_memory || backend.kind == BackendKind::Memory {
                None
            } else {
                Some(khive_runtime::expand_tilde(
                    backend.path.as_deref().ok_or_else(|| {
                        anyhow::anyhow!(
                            "backend {:?}: sqlite backend requires a `path` field",
                            backend.name
                        )
                    })?,
                ))
            };
            targets.push((backend.name.clone(), path));
        }
    }

    let mut reports = Vec::with_capacity(targets.len());
    for (backend, path) in targets {
        let (current_version, validation_error) = match path.as_deref() {
            Some(path) if path.exists() => {
                let version = khive_db::inspect_schema_version(path)
                    .map_err(|error| anyhow::anyhow!("backend {backend}: {error}"))?;
                let validation_error = if version == latest {
                    khive_db::inspect_schema_is_current(path)
                        .err()
                        .map(|error| error.to_string())
                } else {
                    None
                };
                (version, validation_error)
            }
            _ => (0, None),
        };
        let ahead = current_version > latest;
        let is_current = current_version == latest && validation_error.is_none();
        reports.push((
            backend,
            current_version,
            is_current,
            ahead,
            validation_error,
        ));
    }

    if args.human {
        for (backend, current_version, is_current, ahead, validation_error) in &reports {
            let state = if let Some(error) = validation_error {
                format!("invalid current-schema state — {error}")
            } else if *ahead {
                "ahead — incompatible with this build".to_string()
            } else if *is_current {
                "current".to_string()
            } else {
                "behind — run: kkernel db migrate".to_string()
            };
            println!("{backend}: V{current_version} ({state})");
        }
    } else {
        let backends = reports
            .iter()
            .map(
                |(backend, current_version, is_current, ahead, validation_error)| {
                    serde_json::json!({
                        "backend": backend,
                        "current_version": current_version,
                        "latest_version": latest,
                        "current": is_current,
                        "ahead": ahead,
                        "pending": latest.saturating_sub(*current_version),
                        "validation_error": validation_error,
                    })
                },
            )
            .collect::<Vec<_>>();
        println!(
            "{}",
            serde_json::to_string(&serde_json::json!({
                "current": reports.iter().all(|(_, _, current, _, _)| *current),
                "latest_version": latest,
                "backends": backends,
            }))
            .expect("serialize")
        );
    }

    if args.strict && reports.iter().any(|(_, _, current, _, _)| !current) {
        let summary = reports
            .iter()
            .filter(|(_, _, current, _, _)| !current)
            .map(|(backend, version, _, _, validation_error)| {
                validation_error
                    .as_ref()
                    .map(|error| format!("{backend}: V{version}, {error}"))
                    .unwrap_or_else(|| format!("{backend}: V{version}"))
            })
            .collect::<Vec<_>>()
            .join("; ");
        anyhow::bail!(
            "schema topology is not current ({summary}); V{latest} with a complete canonical \
             attachment cutover is required"
        );
    }
    Ok(())
}

/// Writer adapter for process-lifetime diagnostics.
///
/// `tracing-subscriber` reports a failed event write with `eprintln!`. If the
/// subscriber itself writes to a closed stderr pipe, that fallback panics and
/// takes the stdio MCP transport down with it. Logging is auxiliary to the
/// stdin/stdout protocol, so make every stderr write best-effort before it
/// reaches the subscriber's error-reporting path.
struct BestEffortWriter<W>(W);

impl<W: std::io::Write> std::io::Write for BestEffortWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self.0.write(buf) {
            Ok(written) => Ok(written),
            Err(_) => Ok(buf.len()),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        let _ = self.0.flush();
        Ok(())
    }
}

fn init_tracing(level: &str) {
    // Tracing goes to stderr — stdout is reserved for JSON / MCP results.
    //
    // Silence the benign `lattice_inference` tokenizer warning ("tokenizer and
    // model vocab sizes differ" — the multilingual paraphrase model carries a
    // handful of extra reserved tokens) while honoring the caller's level for
    // everything else.
    //
    // Force-enable `khive.boot` at INFO: the resolved-database disclosure
    // (issue #1586) is emitted on that target at startup, and the global
    // default level is `warn` — without this pin the disclosure would be
    // silently filtered for every operator who never sets KHIVE_LOG.
    let filter = format!("{level},khive.boot=info,lattice_inference=error");
    tracing_subscriber::fmt()
        .with_writer(|| BestEffortWriter(std::io::stderr()))
        .with_env_filter(filter)
        .with_ansi(false)
        .init();
}

async fn cmd_sync(args: SyncArgs) -> Result<()> {
    let report = sync::run_sync(&args.repo, &args.db, &args.namespace)
        .await
        .with_context(|| {
            format!(
                "sync failed for repo={} db={}",
                args.repo.display(),
                args.db.display()
            )
        })?;
    let json = serde_json::to_string(&report).expect("serialize SyncReport");
    println!("{json}");
    Ok(())
}

fn cmd_pack(cmd: PackCommand) -> Result<()> {
    match cmd {
        PackCommand::List { human } => {
            let packs = pack_introspect::list_packs()?;
            if human {
                for p in &packs {
                    println!("# {} ({} verbs)", p.name, p.verbs.len());
                    if !p.requires.is_empty() {
                        println!("  requires: {}", p.requires.join(", "));
                    }
                    if !p.note_kinds.is_empty() {
                        println!("  note_kinds:   {}", p.note_kinds.join(", "));
                    }
                    if !p.entity_kinds.is_empty() {
                        println!("  entity_kinds: {}", p.entity_kinds.join(", "));
                    }
                    for v in &p.verbs {
                        println!("    {:<20} {}", v.name, v.description);
                    }
                    println!();
                }
            } else {
                let json = serde_json::to_string(&packs).expect("serialize PackInfo[]");
                println!("{json}");
            }
            Ok(())
        }
        PackCommand::Handler { name, human } => {
            let info = pack_introspect::pack_handler(&name)?;
            let info = info.with_context(|| format!("pack {name:?} is not registered"))?;
            if human {
                println!("# {} ({} verbs)", info.name, info.verbs.len());
                if !info.requires.is_empty() {
                    println!("requires: {}", info.requires.join(", "));
                }
                if !info.note_kinds.is_empty() {
                    println!("note_kinds:   {}", info.note_kinds.join(", "));
                }
                if !info.entity_kinds.is_empty() {
                    println!("entity_kinds: {}", info.entity_kinds.join(", "));
                }
                for v in &info.verbs {
                    println!("  {:<20} {}", v.name, v.description);
                }
            } else {
                let json = serde_json::to_string(&info).expect("serialize PackInfo");
                println!("{json}");
            }
            Ok(())
        }
    }
}

fn cmd_backend(cmd: BackendCommand) -> Result<()> {
    // v1: enumerate backends from RuntimeConfig defaults.
    // Full multi-backend implementation reads khive.toml; this ships
    // the CLI surface so tooling can already call `kkernel backend list`.
    let default_config = RuntimeConfig::default();
    let default_id = default_config.backend_id.clone();
    let default_path = default_config
        .db_path
        .as_ref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| ":memory:".to_string());

    // Build a synthetic registry from the single default backend.
    let mut registry = BackendRegistry::new();
    let rt = KhiveRuntime::new(default_config).map_err(|e| anyhow::anyhow!("{e}"))?;
    registry.register(default_id.clone(), std::sync::Arc::new(rt));

    match cmd {
        BackendCommand::List { human } => {
            let ids: Vec<_> = registry.ids();
            if human {
                println!("Registered backends ({}):", ids.len());
                for id in &ids {
                    let entry = registry.get(id).unwrap();
                    let primary_marker = if registry.primary().map(|p| p.id == *id).unwrap_or(false)
                    {
                        " [primary]"
                    } else {
                        ""
                    };
                    println!("  {}{}", id.as_str(), primary_marker);
                    let _ = entry; // future: print path, file_backed
                }
            } else {
                let names: Vec<&str> = ids.iter().map(|id| id.as_str()).collect();
                let json = serde_json::json!({
                    "backends": names,
                    "primary": registry.primary().map(|e| e.id.as_str()),
                    "count": ids.len(),
                });
                println!("{}", serde_json::to_string(&json).expect("serialize"));
            }
            Ok(())
        }
        BackendCommand::Info { name, human } => {
            let id = BackendId::new(&name);
            let entry = registry
                .get(&id)
                .with_context(|| format!("backend {name:?} is not registered"))?;
            if human {
                let is_primary = registry
                    .primary()
                    .map(|p| p.id == entry.id)
                    .unwrap_or(false);
                println!("backend: {}", entry.id.as_str());
                println!("  primary: {is_primary}");
                println!("  path:    {default_path}");
            } else {
                let json = serde_json::json!({
                    "name": entry.id.as_str(),
                    "path": default_path,
                    "primary": registry.primary().map(|p| p.id == entry.id).unwrap_or(false),
                });
                println!("{}", serde_json::to_string(&json).expect("serialize"));
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;
    use serial_test::serial;
    use tempfile::TempDir;

    fn write_empty_config(dir: &std::path::Path) -> PathBuf {
        let path = dir.join("empty-khive.toml");
        std::fs::write(&path, "").expect("write empty config");
        path
    }

    fn create_v20_fixture(
        path: &std::path::Path,
        legacy: Option<(&str, Option<i64>)>,
    ) -> Option<khive_storage::ContentRef> {
        use khive_db::migrations::{ATTACHMENT_CUTOVER_VERSION, MIGRATIONS};

        let backend = khive_db::StorageBackend::sqlite(path).expect("open V20 fixture backend");
        let mut writer = backend
            .pool()
            .try_writer()
            .expect("open V20 fixture writer");
        let conn = writer.conn_mut();
        conn.execute_batch(
            "CREATE TABLE _schema_migrations (\
                 version INTEGER PRIMARY KEY, \
                 name TEXT NOT NULL, \
                 applied_at INTEGER NOT NULL\
             ) STRICT;",
        )
        .expect("create V20 ledger");
        for migration in MIGRATIONS
            .iter()
            .filter(|migration| migration.version < ATTACHMENT_CUTOVER_VERSION)
        {
            let tx = conn.transaction().expect("begin V20 migration");
            tx.execute_batch(migration.up)
                .unwrap_or_else(|error| panic!("apply V{}: {error}", migration.version));
            tx.execute(
                "INSERT INTO _schema_migrations (version, name, applied_at) \
                 VALUES (?1, ?2, ?3)",
                (
                    migration.version,
                    migration.name,
                    i64::from(migration.version),
                ),
            )
            .unwrap_or_else(|error| panic!("record V{}: {error}", migration.version));
            tx.commit().expect("commit V20 migration");
        }

        legacy.map(|(entity_type, deleted_at)| {
            let content_ref =
                khive_storage::ContentRef::from_hex("a".repeat(64)).expect("canonical legacy ref");
            conn.execute(
                "INSERT INTO entities (\
                     id, namespace, kind, entity_type, name, tags, created_at, updated_at, \
                     deleted_at, content_ref\
                 ) VALUES (?1, 'local', 'artifact', ?2, 'legacy fixture', '[]', 1, 1, ?3, ?4)",
                (
                    uuid::Uuid::new_v4().to_string(),
                    entity_type,
                    deleted_at,
                    content_ref.as_str(),
                ),
            )
            .expect("insert legacy entity");
            content_ref
        })
    }

    fn write_topology_config(
        dir: &std::path::Path,
        main: &std::path::Path,
        secondary: Option<(&str, &std::path::Path)>,
    ) -> PathBuf {
        let path = dir.join("topology.toml");
        let mut config = format!(
            "[[backends]]\nname = \"main\"\nkind = \"sqlite\"\npath = {:?}\n",
            main.display().to_string()
        );
        if let Some((name, secondary)) = secondary {
            config.push_str(&format!(
                "\n[[backends]]\nname = {name:?}\nkind = \"sqlite\"\npath = {:?}\n",
                secondary.display().to_string()
            ));
        }
        config.push_str(&format!(
            "\n[storage.blob]\nbackend = \"fs\"\nroot = {:?}\nfloor_bytes = 0\n",
            dir.join("blobs").display().to_string()
        ));
        std::fs::write(&path, config).expect("write topology config");
        path
    }

    #[test]
    fn version_surfaces_share_the_compiled_provenance() {
        let command = Args::command();

        assert_eq!(command.get_version(), Some(env!("CARGO_PKG_VERSION")));
        assert_eq!(
            command.get_long_version(),
            Some(khive_runtime::BUILD_VERSION)
        );
        assert!(command
            .render_long_version()
            .contains(khive_runtime::BUILD_INFO.source_revision));
        assert!(command
            .render_long_version()
            .contains(khive_runtime::BUILD_INFO.build_time));
    }

    // A schema check must be read-only: it must not create a missing database,
    // and it must not migrate (mutate) an existing one. Regression for the
    // finding that `db check` ran migrations via the read-only runtime path.
    #[tokio::test]
    async fn db_check_does_not_create_missing_file() {
        let tmp = TempDir::new().expect("temp dir");
        let path = tmp.path().join("missing.db");
        let config = write_empty_config(tmp.path());
        assert!(!path.exists());
        cmd_db_check(DbCheckArgs {
            db: Some(path.display().to_string()),
            config: Some(config),
            backend: None,
            strict: false,
            human: false,
        })
        .await
        .expect("db check succeeds on a missing file");
        assert!(!path.exists(), "db check must not create the database file");
    }

    // An explicit `--config` that fails to parse must fail the `kkernel mcp`
    // boot loud — silently defaulting would serve against a config the
    // operator did not select (ADR-035).
    #[tokio::test]
    async fn mcp_fails_loud_when_explicit_config_is_invalid() {
        let tmp = TempDir::new().expect("temp dir");
        let config_path = tmp.path().join("broken.toml");
        std::fs::write(&config_path, "this is not [valid toml\n").expect("write malformed config");

        let mcp_args = khive_mcp::args::Args {
            db: Some(":memory:".to_string()),
            actor: None,
            namespace: None,
            no_embed: true,
            pack: Vec::new(),
            config: Some(config_path.clone()),
            daemon: false,
            transport: None,
            bind: None,
            brain_profile: None,
            resumed_generation: None,
        };

        // The exact load the `Command::Mcp` branch performs first.
        let db_path_hint = khive_mcp::serve::config_discovery_db_anchor(mcp_args.db.as_deref());
        let loaded_config = match KhiveConfig::load_with_home_fallback_and_source(
            mcp_args.config.as_deref(),
            db_path_hint.as_deref(),
        ) {
            Ok(loaded) => loaded,
            Err(load_error) => {
                assert!(
                    mcp_args.config.is_some(),
                    "this test always passes an explicit --config"
                );
                let error: Result<()> =
                    Err(load_error).context("failed to load the explicitly selected config file");
                let error = error.expect_err("an invalid explicit config must fail the boot");
                let rendered = format!("{error:#}");
                assert!(
                    rendered.contains("failed to load the explicitly selected config file"),
                    "the failure must name the explicit selection: {rendered}"
                );
                return;
            }
        };
        panic!(
            "an invalid explicit --config must never reach the tolerant default: {loaded_config:?}"
        );
    }

    // An explicit `--config` naming a nonexistent file must fail loud at the
    // loader — never silently fall through to the discovery tiers (ADR-035).
    // The `Command::Mcp` branch's `Err` arm ("failed to load the explicitly
    // selected config file") carries this case now that the explicit tier is
    // enforced inside `load_with_home_fallback_and_source` itself.
    #[tokio::test]
    async fn mcp_explicit_missing_config_does_not_fall_through_to_discovery() {
        let tmp = TempDir::new().expect("temp dir");
        // Plant a discoverable config in the db-anchored tier
        // (`<db_dir>/.khive/config.toml`). It is the CONTROL proving discovery
        // was NOT consulted: if the explicit tier fell through, discovery
        // would pick this file up and `Ok(Some(...))` would come back instead
        // of the loud missing-file error.
        let anchor_dir = tmp.path().join(".khive");
        std::fs::create_dir_all(&anchor_dir).expect("create discovery anchor dir");
        std::fs::write(anchor_dir.join("config.toml"), "").expect("write discoverable config");
        let missing = tmp.path().join("does-not-exist.toml");
        let db_hint = tmp.path().join("khive.db");

        let error = KhiveConfig::load_with_home_fallback_and_source(Some(&missing), Some(&db_hint))
            .expect_err("an explicit selection naming a missing file must fail loud");
        assert!(
            matches!(
                error,
                khive_runtime::engine_config::ConfigError::ExplicitConfigMissing { .. }
            ),
            "the explicit tier must fail with ExplicitConfigMissing, never fall \
             through to the planted discoverable config: {error:?}"
        );
        let rendered = error.to_string();
        assert!(
            rendered.contains("does-not-exist.toml"),
            "the error must name the missing file the operator selected: {rendered}"
        );
    }

    #[tokio::test]
    async fn db_check_does_not_mutate_existing_db() {
        let tmp = TempDir::new().expect("temp dir");
        let path = tmp.path().join("real.db");
        let config = write_empty_config(tmp.path());
        cmd_db_migrate(DbMigrateArgs {
            db: Some(path.display().to_string()),
            config: Some(config.clone()),
            backend: None,
            dry_run: false,
            check: false,
            human: false,
        })
        .await
        .expect("migrate creates the database");
        // SQLite connection close is deferred, so the migrate above can leave
        // writable `-wal`/`-shm` sidecars behind at this point; the read-only
        // inspection below accepts only the frozen snapshot form.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["-wal", "-shm"] {
                let mut name = path.file_name().expect("db file name").to_os_string();
                name.push(suffix);
                let sidecar = path.parent().expect("db parent dir").join(name);
                if sidecar.exists() {
                    let mut permissions = std::fs::metadata(&sidecar)
                        .expect("sidecar metadata")
                        .permissions();
                    permissions.set_mode(0o444);
                    std::fs::set_permissions(&sidecar, permissions).expect("freeze sidecar");
                }
            }
        }
        let before = std::fs::read(&path).expect("read db before check");
        // strict passes only when the db is already current — proves the read sees V1.
        cmd_db_check(DbCheckArgs {
            db: Some(path.display().to_string()),
            config: Some(config),
            backend: None,
            strict: true,
            human: false,
        })
        .await
        .expect("db check passes on a current db");
        let after = std::fs::read(&path).expect("read db after check");
        assert_eq!(before, after, "db check must not mutate the database");
    }

    #[tokio::test]
    async fn db_check_strict_rejects_noncanonical_v21_without_mutating_it() {
        let tmp = TempDir::new().expect("temp dir");
        let path = tmp.path().join("corrupt-v21.db");
        let config = write_empty_config(tmp.path());
        let backend = khive_db::StorageBackend::sqlite(&path).expect("open fixture backend");
        backend
            .prepare_core_schema()
            .expect("prepare canonical V21");
        {
            let mut writer = backend.pool().try_writer().expect("tamper V21 fixture");
            writer
                .conn_mut()
                .execute("DELETE FROM attachment_cutover_state", [])
                .expect("remove completion marker");
            writer
                .conn_mut()
                .execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .expect("checkpoint tampered fixture");
        }
        drop(backend);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for suffix in ["-wal", "-shm"] {
                let mut name = path.file_name().unwrap().to_os_string();
                name.push(suffix);
                let sidecar = path.parent().unwrap().join(name);
                if sidecar.exists() {
                    let mut permissions = std::fs::metadata(&sidecar).unwrap().permissions();
                    permissions.set_mode(0o444);
                    std::fs::set_permissions(&sidecar, permissions).unwrap();
                }
            }
        }
        let before = std::fs::read(&path).expect("read corrupt fixture");

        let error = cmd_db_check(DbCheckArgs {
            db: Some(path.display().to_string()),
            config: Some(config),
            backend: None,
            strict: true,
            human: false,
        })
        .await
        .expect_err("MAX(version)=21 cannot hide missing physical cutover state");
        assert!(
            error.to_string().contains("attachment cutover"),
            "unexpected strict-check error: {error:#}"
        );
        assert_eq!(
            before,
            std::fs::read(&path).expect("read fixture after check"),
            "strict validation must remain read-only"
        );
    }

    #[tokio::test]
    async fn db_check_main_includes_every_secondary_prerequisite() {
        let tmp = TempDir::new().expect("temp dir");
        let main = tmp.path().join("main.db");
        let secondary = tmp.path().join("secondary.db");
        let main_backend = khive_db::StorageBackend::sqlite(&main).expect("open main fixture");
        main_backend
            .prepare_core_schema()
            .expect("prepare current main fixture");
        drop(main_backend);
        create_v20_fixture(&secondary, None);
        let config =
            write_topology_config(tmp.path(), &main, Some(("secondary", secondary.as_path())));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            for path in [&main, &secondary] {
                for suffix in ["-wal", "-shm"] {
                    let mut name = path.file_name().unwrap().to_os_string();
                    name.push(suffix);
                    let sidecar = path.parent().unwrap().join(name);
                    if sidecar.exists() {
                        let mut permissions = std::fs::metadata(&sidecar).unwrap().permissions();
                        permissions.set_mode(0o444);
                        std::fs::set_permissions(&sidecar, permissions).unwrap();
                    }
                }
            }
        }

        let error = cmd_db_check(DbCheckArgs {
            db: None,
            config: Some(config),
            backend: Some(BackendId::MAIN.to_string()),
            strict: true,
            human: false,
        })
        .await
        .expect_err("a current main cannot hide a behind secondary prerequisite");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("secondary"), "{rendered}");
        assert!(rendered.contains("V20"), "{rendered}");
    }

    #[tokio::test]
    async fn db_migrate_runs_the_application_assisted_v21_cutover() {
        use khive_db::migrations::{ATTACHMENT_CUTOVER_VERSION, MIGRATIONS};
        use khive_db::StorageBackend;
        use khive_storage::ContentRef;

        let tmp = TempDir::new().expect("temp dir");
        let path = tmp.path().join("legacy-v20.db");
        let config = write_empty_config(tmp.path());
        let entity_id = uuid::Uuid::new_v4();
        let content_ref = ContentRef::from_hex("a".repeat(64)).expect("canonical fixture ref");

        let backend = StorageBackend::sqlite(&path).expect("open V20 fixture backend");
        {
            let mut writer = backend.pool().try_writer().expect("V20 fixture writer");
            let conn = writer.conn_mut();
            conn.execute_batch(
                "CREATE TABLE _schema_migrations (\
                     version INTEGER PRIMARY KEY, \
                     name TEXT NOT NULL, \
                     applied_at INTEGER NOT NULL\
                 ) STRICT;",
            )
            .expect("create V20 migration ledger");
            for migration in MIGRATIONS
                .iter()
                .filter(|migration| migration.version < ATTACHMENT_CUTOVER_VERSION)
            {
                let tx = conn.transaction().expect("begin V20 fixture migration");
                tx.execute_batch(migration.up)
                    .unwrap_or_else(|error| panic!("apply V{}: {error}", migration.version));
                tx.execute(
                    "INSERT INTO _schema_migrations (version, name, applied_at) \
                     VALUES (?1, ?2, ?3)",
                    (
                        migration.version,
                        migration.name,
                        i64::from(migration.version),
                    ),
                )
                .unwrap_or_else(|error| panic!("record V{}: {error}", migration.version));
                tx.commit().expect("commit V20 fixture migration");
            }
            conn.execute(
                "INSERT INTO entities (\
                     id, namespace, kind, entity_type, name, tags, created_at, updated_at, \
                     content_ref\
                 ) VALUES (?1, 'local', 'artifact', 'visual_asset', 'legacy visual', '[]', \
                     1, 1, ?2)",
                (entity_id.to_string(), content_ref.as_str()),
            )
            .expect("insert legacy attachment-bearing entity");
        }
        drop(backend);

        cmd_db_migrate(DbMigrateArgs {
            db: Some(path.display().to_string()),
            config: Some(config),
            backend: None,
            dry_run: false,
            check: false,
            human: false,
        })
        .await
        .expect("admin migrate must coordinate V21 rather than stop at V20");

        let migrated = StorageBackend::sqlite(&path).expect("reopen migrated database");
        assert_eq!(
            migrated.prepare_core_schema().unwrap(),
            khive_db::migrations::latest_schema_version()
        );
        let attachment = migrated
            .attachments()
            .expect("V21 attachment store")
            .get_attachment(entity_id, "content")
            .await
            .unwrap()
            .expect("legacy content role must be backfilled");
        assert_eq!(attachment.content_ref, content_ref);
    }

    #[tokio::test]
    async fn db_migrate_preflights_soft_deleted_secondary_before_advancing_main() {
        use khive_db::migrations::{
            attachment_cutover_status, read_schema_version, AttachmentCutoverStatus,
            ATTACHMENT_CUTOVER_VERSION,
        };

        let tmp = TempDir::new().expect("temp dir");
        let main = tmp.path().join("main.db");
        let secondary = tmp.path().join("secondary.db");
        create_v20_fixture(&main, None);
        create_v20_fixture(&secondary, Some(("visual_asset", Some(9))));
        let config =
            write_topology_config(tmp.path(), &main, Some(("secondary", secondary.as_path())));

        let error = cmd_db_migrate(DbMigrateArgs {
            db: None,
            config: Some(config.clone()),
            backend: None,
            dry_run: false,
            check: false,
            human: false,
        })
        .await
        .expect_err("secondary liveness must block main cutover");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("secondary"), "{rendered}");

        let main_backend =
            khive_db::StorageBackend::sqlite(&main).expect("inspect blocked main backend");
        let main_conn = main_backend.pool().reader().expect("inspect blocked main");
        assert_eq!(
            read_schema_version(main_conn.conn()).unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1,
            "main must remain V20 when secondary inventory fails"
        );
        assert_eq!(
            attachment_cutover_status(main_conn.conn()).unwrap(),
            AttachmentCutoverStatus::Pending
        );
        drop(main_conn);
        drop(main_backend);

        let secondary_backend =
            khive_db::StorageBackend::sqlite(&secondary).expect("curate blocked secondary");
        secondary_backend
            .pool()
            .try_writer()
            .expect("secondary writer")
            .conn_mut()
            .execute("UPDATE entities SET content_ref = NULL", [])
            .expect("relocate legacy secondary attachment authority");
        drop(secondary_backend);

        cmd_db_migrate(DbMigrateArgs {
            db: None,
            config: Some(config),
            backend: None,
            dry_run: false,
            check: false,
            human: false,
        })
        .await
        .expect("curated topology must complete secondary then main");
        for path in [&secondary, &main] {
            let backend =
                khive_db::StorageBackend::sqlite(path).expect("inspect completed topology backend");
            let conn = backend.pool().reader().expect("inspect completed topology");
            assert_eq!(
                read_schema_version(conn.conn()).unwrap(),
                khive_db::migrations::latest_schema_version()
            );
            assert_eq!(
                attachment_cutover_status(conn.conn()).unwrap(),
                AttachmentCutoverStatus::Complete
            );
        }
    }

    #[tokio::test]
    async fn db_migrate_named_secondary_does_not_advance_main() {
        use khive_db::migrations::{read_schema_version, ATTACHMENT_CUTOVER_VERSION};

        let tmp = TempDir::new().expect("temp dir");
        let main = tmp.path().join("main.db");
        let secondary = tmp.path().join("secondary.db");
        create_v20_fixture(&main, None);
        create_v20_fixture(&secondary, None);
        let config =
            write_topology_config(tmp.path(), &main, Some(("secondary", secondary.as_path())));

        cmd_db_migrate(DbMigrateArgs {
            db: None,
            config: Some(config),
            backend: Some("secondary".to_string()),
            dry_run: false,
            check: false,
            human: false,
        })
        .await
        .expect("named empty secondary migration");

        let main_backend = khive_db::StorageBackend::sqlite(&main).unwrap();
        let secondary_backend = khive_db::StorageBackend::sqlite(&secondary).unwrap();
        let main_conn = main_backend.pool().reader().unwrap();
        let secondary_conn = secondary_backend.pool().reader().unwrap();
        assert_eq!(
            read_schema_version(main_conn.conn()).unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1
        );
        assert_eq!(
            read_schema_version(secondary_conn.conn()).unwrap(),
            khive_db::migrations::latest_schema_version()
        );
    }

    #[tokio::test]
    async fn db_migrate_unknown_backend_creates_no_database() {
        let tmp = TempDir::new().expect("temp dir");
        let main = tmp.path().join("missing-main.db");
        let secondary = tmp.path().join("missing-secondary.db");
        let config =
            write_topology_config(tmp.path(), &main, Some(("secondary", secondary.as_path())));

        let error = cmd_db_migrate(DbMigrateArgs {
            db: None,
            config: Some(config),
            backend: Some("missing".to_string()),
            dry_run: false,
            check: false,
            human: false,
        })
        .await
        .expect_err("unknown selector must fail before opening topology");
        let rendered = format!("{error:#}");
        assert!(rendered.contains("unknown backend"), "{rendered}");
        assert!(!main.exists());
        assert!(!secondary.exists());
    }

    #[tokio::test]
    async fn db_migrate_one_declared_main_uses_its_configured_path() {
        use khive_db::migrations::read_schema_version;

        let tmp = TempDir::new().expect("temp dir");
        let main = tmp.path().join("declared-main.db");
        create_v20_fixture(&main, None);
        let config = write_topology_config(tmp.path(), &main, None);

        cmd_db_migrate(DbMigrateArgs {
            db: None,
            config: Some(config),
            backend: None,
            dry_run: false,
            check: false,
            human: false,
        })
        .await
        .expect("one declared main must use topology path");
        let backend = khive_db::StorageBackend::sqlite(&main).unwrap();
        let conn = backend.pool().reader().unwrap();
        assert_eq!(
            read_schema_version(conn.conn()).unwrap(),
            khive_db::migrations::latest_schema_version()
        );
    }

    // --- `-e` quick-shot shortcut for `exec` ---

    #[test]
    fn exec_shortcut_short_flag_parses_ops() {
        let args = Args::parse_from(["kkernel", "-e", "stats()"]);
        assert_eq!(args.exec.as_deref(), Some("stats()"));
        assert!(args.command.is_none());
    }

    #[test]
    fn exec_shortcut_long_flag_parses_ops() {
        let args = Args::parse_from(["kkernel", "--exec", "stats()"]);
        assert_eq!(args.exec.as_deref(), Some("stats()"));
        assert!(args.command.is_none());
    }

    // `-e` and a subcommand both parse fine individually at the clap level
    // (clap has no way to declare a `#[command(subcommand)]` field as a
    // `conflicts_with` target — see `resolve_command_result`'s doc comment),
    // so `kkernel -e 'x()' exec 'y()'` parses into `Args { exec: Some(..),
    // command: Some(..) }` without a clap-level error. The conflict is instead
    // enforced by `resolve_command_result`, exercised directly below (its
    // process-exiting `main()` wrapper, `resolve_command`, is not unit-testable).
    #[test]
    fn exec_shortcut_conflicts_with_subcommand() {
        let args = Args::parse_from(["kkernel", "-e", "stats()", "exec", "other()"]);
        assert!(args.exec.is_some());
        assert!(args.command.is_some());

        let result = resolve_command_result(args.exec, args.command);
        assert!(matches!(result, Err(ResolveCommandError::Conflict)));
    }

    #[test]
    fn exec_shortcut_conflicts_with_subcommand_reverse_order() {
        // Subcommand first, -e after — still rejected, though for a different
        // reason than the `-e ... exec ...` order above: once `exec` starts
        // consuming tokens, `-e` is not one of `ExecArgs`'s own flags, so this
        // is a genuine clap-level "unexpected argument" parse error rather than
        // reaching `resolve_command_result`'s conflict branch. Either order is
        // still rejected, which is the acceptance-relevant behavior.
        let bare = Args::try_parse_from(["kkernel", "pack", "list"]);
        assert!(bare.is_ok(), "a bare subcommand alone must still parse");

        let result = Args::try_parse_from(["kkernel", "exec", "other()", "-e", "stats()"]);
        assert!(
            result.is_err(),
            "-e after a subcommand's own args must be rejected"
        );
    }

    #[test]
    fn resolve_command_result_missing_when_neither_given() {
        let args = Args::parse_from(["kkernel"]);
        let result = resolve_command_result(args.exec, args.command);
        assert!(matches!(result, Err(ResolveCommandError::Missing)));
    }

    #[test]
    fn resolve_command_result_exec_only_maps_to_exec_command() {
        let args = Args::parse_from(["kkernel", "-e", "stats()"]);
        let result = resolve_command_result(args.exec, args.command);
        match result {
            Ok(Command::Exec(e)) => assert_eq!(e.ops.as_deref(), Some("stats()")),
            other => panic!("expected Ok(Command::Exec), got {other:?}"),
        }
    }

    #[test]
    fn exec_shortcut_maps_to_same_ops_as_exec_subcommand() {
        // `-e '<ops>'` must produce the identical ExecArgs the `exec` subcommand
        // itself would parse from `exec '<ops>'` — the resolution logic in
        // `main()` builds this via `exec::ExecArgs::parse_from(["exec", "--", &ops])`.
        let via_shortcut = match resolve_command_result(Some("stats()".into()), None) {
            Ok(Command::Exec(e)) => e,
            other => panic!("expected Ok(Command::Exec), got {other:?}"),
        };
        let via_subcommand = match Args::parse_from(["kkernel", "exec", "stats()"]).command {
            Some(Command::Exec(e)) => e,
            other => panic!("expected Command::Exec, got {other:?}"),
        };
        assert_eq!(via_shortcut.ops, via_subcommand.ops);
        assert_eq!(via_shortcut.db, via_subcommand.db);
        assert_eq!(via_shortcut.namespace, via_subcommand.namespace);
        assert_eq!(via_shortcut.presentation, via_subcommand.presentation);
    }

    #[test]
    fn exec_shortcut_flag_like_ops_binds_as_ops_not_as_exec_flag() {
        // Regression: without the `--` separator
        // in the synthetic argv, `-e '--pending-events'` reparsed as exec's
        // `--pending-events` FLAG (running the pending-event drain with no ops)
        // instead of binding as the OPS value.
        let resolved = match resolve_command_result(Some("--pending-events".into()), None) {
            Ok(Command::Exec(e)) => e,
            other => panic!("expected Ok(Command::Exec), got {other:?}"),
        };
        assert_eq!(resolved.ops.as_deref(), Some("--pending-events"));
        assert!(!resolved.pending_events);
    }

    #[test]
    fn bare_invocation_without_exec_or_subcommand_is_not_a_valid_parse_state() {
        // clap itself allows `kkernel` with neither -e nor a subcommand to parse
        // (both are optional at the clap level); main() is what turns that into
        // an error via `Args::command().error(...).exit()`. This test pins the
        // parse-level shape that main() branches on.
        let args = Args::parse_from(["kkernel"]);
        assert!(args.exec.is_none());
        assert!(args.command.is_none());
    }

    // --- #603: multi-backend boot path consolidation ---
    //
    // Both multi-backend boot paths — `khive_mcp::serve::build_server_multi_backend`
    // (the plain `kkernel mcp` path, no coordinator) and this crate's
    // `build_multi_backend_server_with_coordinator` (the `Command::Mcp` coordinator
    // branch) — now finish through the SAME shared constructor
    // (`khive_mcp::serve::build_server_from_multi_backend_registry`). Before #603,
    // the coordinator branch hand-copied registry assembly, the output-format
    // resolution, and the checkpoint-pool wiring inline in `main()`, and missed
    // wiring three times as each was patched independently (#503, ADR-078, #601).
    // This test drives BOTH real production entry points (not a hand-reimplementation
    // of either) against the same config and asserts their `WiringSurface`s match —
    // the regression this consolidation exists to prevent is exactly two boot paths
    // silently drifting apart again.

    fn base_multi_backend_runtime_config() -> RuntimeConfig {
        use khive_runtime::Namespace;
        // Callers that construct a server with no explicit DB override must be
        // `#[serial]`: both this resolver and the construction guard read the
        // process-wide HOME, which another CLI test deliberately mutates.
        RuntimeConfig {
            // Matches what `resolve_runtime_config` would set for a `--db`-unset
            // invocation (the `cli_db_override: None` every call site below
            // passes) — `build_server_multi_backend`'s db-anchor consistency
            // guard requires `db_path` to agree with `resolve_db_anchor` for
            // the same input.
            db_path: khive_runtime::resolve_db_anchor(None),
            default_namespace: Namespace::parse("local").expect("valid namespace"),
            embedding_model: None,
            additional_embedding_models: vec![],
            packs: vec!["kg".to_string()],
            backend_id: BackendId::main(),
            ..RuntimeConfig::default()
        }
    }

    fn single_main_backend_config(
        kind: khive_runtime::BackendKind,
        path: Option<PathBuf>,
    ) -> KhiveConfig {
        KhiveConfig {
            backends: vec![khive_runtime::BackendConfig {
                name: "main".to_string(),
                kind,
                path,
                cache_mb: None,
                journal_mode: None,
                read_only: false,
            }],
            ..KhiveConfig::default()
        }
    }

    /// File-backed main: both boot paths must agree on every `WiringSurface`
    /// field — in particular, both must wire a checkpoint pool (#601/#604).
    #[tokio::test]
    #[serial]
    async fn multi_backend_boot_paths_share_identical_wiring_surface_file_backed() {
        let dir = TempDir::new().expect("temp dir");
        let main_path = dir.path().join("main.db");
        let khive_cfg =
            single_main_backend_config(khive_runtime::BackendKind::Sqlite, Some(main_path));

        let plain_server = khive_mcp::serve::build_server_multi_backend(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
        .await
        .expect("plain multi-backend boot must succeed");

        let (coordinator_server, _schedule_rt) = build_multi_backend_server_with_coordinator(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
        .await
        .expect("kkernel coordinator-attached multi-backend boot must succeed");

        let plain_surface = khive_mcp::serve::WiringSurface::capture(&plain_server);
        let coordinator_surface = khive_mcp::serve::WiringSurface::capture(&coordinator_server);

        assert_eq!(
            plain_surface, coordinator_surface,
            "the plain multi-backend boot path and kkernel's coordinator-attached \
             boot path must produce an identical wiring surface for the same config"
        );
        assert!(
            plain_surface.has_checkpoint_pool,
            "file-backed main must wire a checkpoint pool on both paths"
        );
    }

    /// In-memory main: both paths must agree that no checkpoint pool is wired
    /// (checkpoint_once must never run on a non-WAL connection).
    #[tokio::test]
    #[serial]
    async fn multi_backend_boot_paths_share_identical_wiring_surface_in_memory() {
        let khive_cfg = single_main_backend_config(khive_runtime::BackendKind::Memory, None);

        let plain_server = khive_mcp::serve::build_server_multi_backend(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
        .await
        .expect("plain multi-backend boot must succeed");

        let (coordinator_server, _schedule_rt) = build_multi_backend_server_with_coordinator(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
        .await
        .expect("kkernel coordinator-attached multi-backend boot must succeed");

        let plain_surface = khive_mcp::serve::WiringSurface::capture(&plain_server);
        let coordinator_surface = khive_mcp::serve::WiringSurface::capture(&coordinator_server);

        assert_eq!(
            plain_surface, coordinator_surface,
            "the plain multi-backend boot path and kkernel's coordinator-attached \
             boot path must produce an identical wiring surface for the same config"
        );
        assert!(
            !plain_surface.has_checkpoint_pool,
            "in-memory main must never carry a checkpoint pool on either path"
        );
    }

    /// #613 non-vacuous output-format parity check — see
    /// `crates/kkernel/docs/coordinator.md#kkernel-mainrs--coordinator-attached-boot-path`.
    #[tokio::test]
    #[serial]
    async fn multi_backend_boot_paths_share_identical_non_default_output_format() {
        // RAII guard: snapshots KHIVE_OUTPUT_FORMAT, clears it, and restores the
        // original value (or leaves it removed) on drop — including on panic, so
        // a failing assertion or an unexpected constructor error never leaks the
        // cleared env var to later #[serial] tests. Mirrors `EmailEnvGuard` in
        // `khive-mcp/src/serve.rs` (#603, PR #613:
        // the prior manual save/clear/restore only ran on the success path).
        struct OutputFormatEnvGuard {
            prev: Option<String>,
        }

        impl OutputFormatEnvGuard {
            fn clear() -> Self {
                let prev = std::env::var("KHIVE_OUTPUT_FORMAT").ok();
                std::env::remove_var("KHIVE_OUTPUT_FORMAT");
                Self { prev }
            }
        }

        impl Drop for OutputFormatEnvGuard {
            fn drop(&mut self) {
                match &self.prev {
                    Some(v) => std::env::set_var("KHIVE_OUTPUT_FORMAT", v),
                    None => std::env::remove_var("KHIVE_OUTPUT_FORMAT"),
                }
            }
        }

        let _env_guard = OutputFormatEnvGuard::clear();

        let mut khive_cfg = single_main_backend_config(khive_runtime::BackendKind::Memory, None);
        khive_cfg.runtime.default_output_format = Some(khive_runtime::OutputFormat::Table);

        let plain_server = khive_mcp::serve::build_server_multi_backend(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
        .await
        .expect("plain multi-backend boot must succeed");

        let (coordinator_server, _schedule_rt) = build_multi_backend_server_with_coordinator(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
        .await
        .expect("kkernel coordinator-attached multi-backend boot must succeed");

        let plain_surface = khive_mcp::serve::WiringSurface::capture(&plain_server);
        let coordinator_surface = khive_mcp::serve::WiringSurface::capture(&coordinator_server);

        assert_eq!(
            plain_surface, coordinator_surface,
            "the plain multi-backend boot path and kkernel's coordinator-attached \
             boot path must produce an identical wiring surface for the same config"
        );
        assert_eq!(
            plain_surface.output_format,
            khive_runtime::OutputFormat::Table,
            "both paths must resolve the configured non-default [runtime].default_output_format \
             (Table), not silently fall back to the builtin Json default — this is the exact \
             ADR-078 regression class the parity test exists to catch"
        );

        // `_env_guard` is dropped here (or on unwind, whichever comes first),
        // restoring KHIVE_OUTPUT_FORMAT regardless of assertion outcome.
    }

    /// db-anchor consistency guard applies at the coordinator choke point too —
    /// see `crates/kkernel/docs/coordinator.md#kkernel-mainrs--coordinator-attached-boot-path`.
    #[tokio::test]
    async fn coordinator_boundary_rejects_diverging_db_path() {
        let args_db = "/tmp/khive-coordinator-guard-real.db";
        let wrong_path = std::path::PathBuf::from("/tmp/khive-coordinator-guard-wrong.db");

        let base_cfg = RuntimeConfig {
            db_path: Some(wrong_path.clone()),
            ..base_multi_backend_runtime_config()
        };
        let khive_cfg = KhiveConfig::default();

        let db_anchor = khive_runtime::resolve_db_anchor(Some(args_db));
        let result = build_multi_backend_server_with_coordinator_and_db_anchor(
            base_cfg,
            &khive_cfg,
            Some(args_db),
            db_anchor.as_deref(),
        )
        .await;

        let err = match result {
            Ok(_) => panic!(
                "a resolved db_path diverging from the canonical anchor must be rejected \
                 at the coordinator-attached construction boundary"
            ),
            Err(e) => e,
        };
        let msg = err.to_string();
        let anchor =
            khive_runtime::resolve_db_anchor(Some(args_db)).expect("explicit path always anchors");
        assert!(
            msg.contains(&wrong_path.display().to_string()),
            "error must name the resolved (wrong) path: {msg}"
        );
        assert!(
            msg.contains(&anchor.display().to_string()),
            "error must name the canonical anchor path: {msg}"
        );
    }

    /// Regression for #720 — see
    /// `crates/kkernel/docs/coordinator.md#kkernel-mainrs--coordinator-attached-boot-path`.
    #[tokio::test]
    #[serial]
    async fn coordinator_boot_uses_anchor_captured_by_runtime_config() {
        struct HomeGuard(Option<std::ffi::OsString>);

        impl Drop for HomeGuard {
            fn drop(&mut self) {
                match &self.0 {
                    Some(home) => std::env::set_var("HOME", home),
                    None => std::env::remove_var("HOME"),
                }
            }
        }

        let original_home = std::env::var_os("HOME");
        let _home_guard = HomeGuard(original_home);
        let first_home = TempDir::new().expect("first HOME");
        std::env::set_var("HOME", first_home.path());
        let config_path = first_home.path().join("config.toml");
        std::fs::write(&config_path, "").expect("write empty config");

        let (base_cfg, db_anchor) = khive_mcp::serve::resolve_runtime_config_with_db_anchor(
            khive_mcp::serve::RuntimeConfigInputs {
                db: None,
                config: Some(&config_path),
                namespace: khive_runtime::Namespace::parse("local").expect("namespace"),
                namespace_explicit: false,
                actor_explicit: false,
                no_embed: true,
                packs: Some(vec!["kg".to_string()]),
                brain_profile: None,
            },
        )
        .expect("resolve runtime config before HOME changes");

        let mut khive_cfg = single_main_backend_config(khive_runtime::BackendKind::Memory, None);
        khive_cfg.backends.push(khive_runtime::BackendConfig {
            name: "secondary".to_string(),
            kind: khive_runtime::BackendKind::Memory,
            path: None,
            cache_mb: None,
            journal_mode: None,
            read_only: false,
        });

        let second_home = TempDir::new().expect("second HOME");
        std::env::set_var("HOME", second_home.path());
        let result = build_multi_backend_server_with_coordinator_and_db_anchor(
            base_cfg,
            &khive_cfg,
            None,
            db_anchor.as_deref(),
        )
        .await;
        if let Err(error) = result {
            panic!(
                "coordinator-attached construction must retain the anchor captured by \
                 resolve_runtime_config instead of re-reading HOME: {error}"
            );
        }
    }

    // --- #674: coordinator link-target resolution parity with `get` ---

    /// Regression for #674 — see
    /// `crates/kkernel/docs/coordinator.md#kkernel-mainrs--coordinator-attached-boot-path`.
    #[tokio::test]
    #[serial]
    async fn coordinator_link_annotates_resolves_edge_target_like_get() {
        use khive_mcp::tools::request::RequestParams;
        use khive_runtime::PackConfig;

        let khive_cfg = KhiveConfig {
            backends: vec![
                khive_runtime::BackendConfig {
                    name: "main".to_string(),
                    kind: khive_runtime::BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
                khive_runtime::BackendConfig {
                    name: "sessions".to_string(),
                    kind: khive_runtime::BackendKind::Memory,
                    path: None,
                    cache_mb: None,
                    journal_mode: None,
                    read_only: false,
                },
            ],
            packs: {
                let mut m = std::collections::HashMap::new();
                m.insert(
                    "session".to_string(),
                    PackConfig {
                        backend: "sessions".to_string(),
                    },
                );
                m
            },
            ..KhiveConfig::default()
        };

        let base_cfg = RuntimeConfig {
            packs: vec!["kg".to_string(), "session".to_string()],
            ..base_multi_backend_runtime_config()
        };

        let (server, _schedule_rt) =
            build_multi_backend_server_with_coordinator(base_cfg, &khive_cfg, None)
                .await
                .expect("coordinator-attached multi-backend boot must succeed");

        let dispatch = |ops: String| {
            let server = &server;
            async move {
                // "verbose" presentation: the bug is specifically about
                // full-UUID `link` endpoints (issue #674) — the default
                // "agent" presentation truncates ids, which would silently
                // route around the coordinator's full-UUID-only interception.
                let resp = server
                    .dispatch_request_local(RequestParams {
                        ops,
                        presentation: Some("verbose".to_string()),
                        presentation_per_op: None,
                        save_to: None,
                        format: None,
                        format_per_op: None,
                        request_id: None,
                    })
                    .await
                    .expect("dispatch must not error");
                serde_json::from_str::<serde_json::Value>(&resp).expect("valid JSON")
            }
        };

        // Two concepts + a link between them to create an edge.
        let a = dispatch(r#"create(kind="concept", name="edge-endpoint-a")"#.to_string()).await;
        let a_id = a["results"][0]["result"]["id"]
            .as_str()
            .expect("create must return an id")
            .to_string();
        let b = dispatch(r#"create(kind="concept", name="edge-endpoint-b")"#.to_string()).await;
        let b_id = b["results"][0]["result"]["id"]
            .as_str()
            .expect("create must return an id")
            .to_string();
        let edge_resp = dispatch(format!(
            r#"link(source_id="{a_id}", target_id="{b_id}", relation="extends")"#
        ))
        .await;
        assert_eq!(
            edge_resp["results"][0]["ok"].as_bool(),
            Some(true),
            "seed edge creation must succeed: {edge_resp}"
        );
        let edge_id = edge_resp["results"][0]["result"]["id"]
            .as_str()
            .expect("link must return an edge id")
            .to_string();

        // A note to use as the annotates source.
        let note_resp =
            dispatch(r#"create(kind="observation", content="annotates source")"#.to_string()).await;
        let note_id = note_resp["results"][0]["result"]["id"]
            .as_str()
            .expect("create must return an id")
            .to_string();

        // Parity check #1: `get` resolves the edge-substrate UUID.
        let got_edge = dispatch(format!(r#"get(id="{edge_id}")"#)).await;
        assert_eq!(
            got_edge["results"][0]["ok"].as_bool(),
            Some(true),
            "get(<edge_uuid>) must succeed: {got_edge}"
        );
        assert_eq!(
            got_edge["results"][0]["result"]["kind"].as_str(),
            Some("edge"),
            "get must resolve the UUID as an edge: {got_edge}"
        );

        // Parity check #2 (the regression): note -> edge `annotates` link
        // through the coordinator-attached multi-backend path must succeed
        // too, resolving the exact same UUID `get` just resolved above.
        let annotate_resp = dispatch(format!(
            r#"link(source_id="{note_id}", target_id="{edge_id}", relation="annotates")"#
        ))
        .await;
        assert_eq!(
            annotate_resp["results"][0]["ok"].as_bool(),
            Some(true),
            "note->edge annotates link must succeed, proving get/link resolution parity \
             for an edge-substrate UUID under multi-backend pack bindings: {annotate_resp}"
        );

        // Parity assertion: the `annotates` link's written target_id is
        // exactly the same UUID `get` resolved as kind=edge above — `get`
        // and `link` endpoint resolution agree for an edge-substrate UUID.
        assert_eq!(
            annotate_resp["results"][0]["result"]["target_id"].as_str(),
            got_edge["results"][0]["result"]["id"].as_str(),
            "link's resolved annotates target must be the exact same edge UUID get() resolved: \
             annotate_resp={annotate_resp} got_edge={got_edge}"
        );
    }
}
