//! `kkernel` binary — khive admin/management Rust CLI.
//!
//! The kernel/MCP split keeps admin and infrastructure operations out of the
//! MCP surface.
//!
//! Subcommands:
//!
//! - `sync`    — build a queryable SQLite DB from NDJSON sources (issue #174)
//! - `pack`    — introspect registered packs (`list`, `handler <name>`)
//! - `kg`      — KG validation, init, hook management
//! - `engine`  — embedding model lifecycle: list/status/migrate/drift-check
//! - `vector`  — vector store capabilities and orphan sweep
//! - `reindex` — rebuild embedding vectors for entities, notes, and the
//!   knowledge corpus (fans out across every configured engine)
//! - `exec`    — run a verb DSL expression through the pack registry
//! - `mcp`     — serve the MCP `request` surface (stdio / daemon / transports)
//! - `backend` — inspect registered backends (`list`, `info <name>`)
//! - `git-ingest` — one-shot batch ingest of commit/issue/pull_request
//!   provenance notes from a local git repository (ADR-088)
//! - `code-ingest`: admin path that validates and ingests a `findings.json`
//!   audit sweep into the graph as `finding` notes (ADR-085 Amendment 3)
//!
//! All subcommands emit JSON on stdout by default for easy piping/parsing.
//! Pass `--human` to switch to a readable table where supported.

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use khive_runtime::{BackendId, KhiveConfig, KhiveRuntime, RuntimeConfig};
use kkernel::{
    code_ingest,
    coordinator::{BackendRegistry, SubstrateCoordinator, SubstrateCoordinatorService},
    engine, exec, git_ingest, kg, pack_introspect, reindex, sync, vector,
};

#[derive(Parser, Debug)]
#[command(
    name = "kkernel",
    version,
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

    /// Inspect registered backends.
    #[command(subcommand)]
    Backend(BackendCommand),

    /// One-shot batch ingest of commit/issue/pull_request provenance notes
    /// from a local git repository (ADR-088).
    GitIngest(git_ingest::GitIngestArgs),

    /// Validate and ingest a `findings.json` audit sweep into the graph as
    /// `finding` notes (ADR-085 Amendment 3).
    CodeIngest(code_ingest::CodeIngestArgs),
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
    #[arg(long)]
    db: Option<PathBuf>,

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
    #[arg(long)]
    db: Option<PathBuf>,

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

#[tokio::main]
async fn main() -> Result<()> {
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
        Command::Db(d) => cmd_db(d).await,
        Command::Engine(e) => engine::run_engine(e).await,
        Command::Vector(v) => vector::run_vector(v),
        Command::Reindex(r) => reindex::run_reindex(r).await,
        Command::Exec(e) => exec::run_exec(e).await,
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
            let khive_cfg =
                KhiveConfig::load_with_home_fallback(a.config.as_deref(), db_path_hint.as_deref())
                    .unwrap_or_default()
                    .unwrap_or_default();

            if khive_cfg.backends.len() <= 1 {
                // Single-backend: zero-change path — no coordinator.
                khive_mcp::serve::run(a, &transport_registry).await
            } else {
                // Multi-backend: build registry, attach the SubstrateCoordinator,
                // and finish server assembly through the shared #603 constructor
                // (`build_multi_backend_server_with_coordinator`) — this branch
                // contains no server-assembly logic of its own beyond building
                // the coordinator inputs (BackendRegistry + note_kinds) and
                // attaching it.
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
                    )?;

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
/// top-level entry points: the `-e/--exec` quick-shot flag and a subcommand.
/// Split out from [`resolve_command`] so the four cases are unit-testable
/// without triggering `clap`'s process-exiting `.error(...).exit()` path.
///
/// - `-e <OPS>` alone → `Command::Exec`, parsed via `ExecArgs::parse_from(["exec",
///   "--", &ops])` so it is byte-identical to typing `exec -- <OPS>` (same
///   defaults, same env-var bindings). The `--` separator forces `ops` to bind
///   as the positional OPS value even when it starts with `-` (without it,
///   `-e '--pending-events'` would reparse as exec's `--pending-events` flag).
/// - a subcommand alone → that subcommand, unchanged.
/// - neither → `Err(ResolveCommandError::Missing)`.
/// - both → `Err(ResolveCommandError::Conflict)`. clap's derive `conflicts_with`
///   cannot name a `#[command(subcommand)]` field directly (it is not a plain
///   `Arg` — confirmed via clap's own startup debug_assert), so this case is
///   enforced here rather than declaratively on the field.
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

/// Build the coordinator-attached multi-backend server for `kkernel mcp`
/// (the `Command::Mcp` branch, taken when `[[backends]]` declares more than
/// one backend).
///
/// Extracted out of `main()` (#603) so this is the ONE place that assembles
/// the `BackendRegistry`/`SubstrateCoordinator` inputs and hands them to the
/// shared `khive_mcp::serve::build_server_from_multi_backend_registry`
/// constructor — everything but that coordinator attachment (registry
/// assembly, the ADR-078 output format, the ADR-091 checkpoint pool) now
/// lives in exactly one place in `khive-mcp::serve`, not hand-copied here.
/// Extraction also makes this branch directly comparable, in a test, against
/// `khive_mcp::serve::build_server_multi_backend` (the sibling boot path) —
/// see `multi_backend_boot_paths_share_identical_wiring_surface` below.
///
/// Also returns the resolved `"schedule"`-pack runtime (ADR-106), read out of
/// the same `multi.per_pack_runtimes` map used to build the coordinator's
/// `BackendRegistry` below — `None` when the resolved pack set does not
/// include `"schedule"`. The caller threads this through
/// `khive_mcp::serve::serve_server` so the daemon-resident tick loop
/// (`spawn_schedule_tick_loop_if_daemon`) drains the exact backend this
/// coordinator-attached boot resolved, never a re-derived config (PR
/// #782).
#[cfg(test)]
fn build_multi_backend_server_with_coordinator(
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
}

fn build_multi_backend_server_with_coordinator_and_db_anchor(
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
    )?;

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

    let note_kinds: std::collections::HashSet<String> = multi
        .registry
        .all_note_kinds()
        .into_iter()
        .map(str::to_string)
        .collect();
    let coord =
        SubstrateCoordinatorService::new(SubstrateCoordinator::new(backend_reg), note_kinds);

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

async fn cmd_db_migrate(args: DbMigrateArgs) -> Result<()> {
    // KhiveRuntime::new() runs run_migrations() internally.
    // Constructing the runtime is therefore sufficient to apply all pending migrations.
    let mut cfg = RuntimeConfig::default();
    if let Some(ref db) = args.db {
        cfg.db_path = Some(db.clone());
    }

    if args.dry_run || args.check {
        // For dry-run / --check, query the current schema version without writing.
        return cmd_db_check(DbCheckArgs {
            db: args.db,
            strict: args.check,
            human: args.human,
        })
        .await;
    }

    let rt = KhiveRuntime::new(cfg).map_err(|e| anyhow::anyhow!("{e}"))?;
    let latest = khive_db::MIGRATIONS.len() as u32;

    // Query the applied version to report what was done.
    let sql = rt.sql();
    let mut reader = sql
        .reader()
        .await
        .context("open SQL reader after migration")?;
    use khive_storage::types::{SqlStatement, SqlValue};
    let rows = reader
        .query_all(SqlStatement {
            sql: "SELECT COALESCE(MAX(version), 0) FROM _schema_migrations".into(),
            params: vec![],
            label: Some("db_migrate_version".into()),
        })
        .await
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let applied: u32 = rows
        .first()
        .and_then(|r| match r.get("COALESCE(MAX(version), 0)") {
            Some(SqlValue::Integer(v)) => Some(*v as u32),
            _ => None,
        })
        .unwrap_or(latest);

    if args.human {
        println!("schema migrated: version {applied} of {latest} (current)");
    } else {
        let json = serde_json::json!({
            "applied_version": applied,
            "latest_version": latest,
            "current": applied == latest,
        });
        println!("{}", serde_json::to_string(&json).expect("serialize"));
    }
    Ok(())
}

async fn cmd_db_check(args: DbCheckArgs) -> Result<()> {
    let latest = khive_db::MIGRATIONS.len() as u32;

    // A schema check must never mutate the database. Resolve the effective path
    // and read `_schema_migrations` read-only — opening through a runtime would
    // run migrations and bring an out-of-date database current before reporting,
    // masking the pending state this command exists to detect.
    let resolved: Option<PathBuf> = match args.db {
        Some(p) => Some(p),
        None => std::env::var("HOME")
            .ok()
            .map(|h| PathBuf::from(h).join(".khive/khive.db")),
    };

    // An absent file is an un-migrated database (version 0); do not create it.
    let current_version: u32 = match resolved {
        Some(ref p) if p.exists() => {
            khive_db::inspect_schema_version(p).map_err(|e| anyhow::anyhow!("{e}"))?
        }
        _ => 0,
    };

    let is_current = current_version == latest;
    // A version beyond the latest known migration is a stale ledger: the database
    // predates the consolidated V1 baseline (ADR-015) or was written by a newer
    // build. Report it rather than treating it as current.
    let ahead = current_version > latest;

    if args.human {
        let state = if ahead {
            "ahead — predates the consolidated baseline (ADR-015) or written by a newer build; recreate it"
        } else if is_current {
            "current"
        } else {
            "behind — run: kkernel db migrate"
        };
        println!("main:    V{current_version} ({state})");
    } else {
        let json = serde_json::json!({
            "current_version": current_version,
            "latest_version": latest,
            "current": is_current,
            "ahead": ahead,
            "pending": latest.saturating_sub(current_version),
        });
        println!("{}", serde_json::to_string(&json).expect("serialize"));
    }

    if args.strict && !is_current {
        if ahead {
            anyhow::bail!(
                "schema version {current_version} is ahead of the latest known migration {latest} — \
                 this database predates the consolidated baseline (ADR-015) or was written by a newer \
                 build; recreate it from the current schema"
            );
        }
        anyhow::bail!(
            "schema is behind: V{current_version} applied, V{latest} is current — \
             run `kkernel db migrate` to bring the schema up to date"
        );
    }
    Ok(())
}

fn init_tracing(level: &str) {
    // Tracing goes to stderr — stdout is reserved for JSON / MCP results.
    //
    // Silence the benign `lattice_inference` tokenizer warning ("tokenizer and
    // model vocab sizes differ" — the multilingual paraphrase model carries a
    // handful of extra reserved tokens) while honoring the caller's level for
    // everything else.
    let filter = format!("{level},lattice_inference=error");
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
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
    use serial_test::serial;
    use tempfile::TempDir;

    // A schema check must be read-only: it must not create a missing database,
    // and it must not migrate (mutate) an existing one. Regression for the
    // finding that `db check` ran migrations via the read-only runtime path.
    #[tokio::test]
    async fn db_check_does_not_create_missing_file() {
        let tmp = TempDir::new().expect("temp dir");
        let path = tmp.path().join("missing.db");
        assert!(!path.exists());
        cmd_db_check(DbCheckArgs {
            db: Some(path.clone()),
            strict: false,
            human: false,
        })
        .await
        .expect("db check succeeds on a missing file");
        assert!(!path.exists(), "db check must not create the database file");
    }

    #[tokio::test]
    async fn db_check_does_not_mutate_existing_db() {
        let tmp = TempDir::new().expect("temp dir");
        let path = tmp.path().join("real.db");
        cmd_db_migrate(DbMigrateArgs {
            db: Some(path.clone()),
            backend: None,
            dry_run: false,
            check: false,
            human: false,
        })
        .await
        .expect("migrate creates the database");
        let before = std::fs::read(&path).expect("read db before check");
        // strict passes only when the db is already current — proves the read sees V1.
        cmd_db_check(DbCheckArgs {
            db: Some(path.clone()),
            strict: true,
            human: false,
        })
        .await
        .expect("db check passes on a current db");
        let after = std::fs::read(&path).expect("read db after check");
        assert_eq!(before, after, "db check must not mutate the database");
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
    #[test]
    fn multi_backend_boot_paths_share_identical_wiring_surface_file_backed() {
        let dir = TempDir::new().expect("temp dir");
        let main_path = dir.path().join("main.db");
        let khive_cfg =
            single_main_backend_config(khive_runtime::BackendKind::Sqlite, Some(main_path));

        let plain_server = khive_mcp::serve::build_server_multi_backend(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
        .expect("plain multi-backend boot must succeed");

        let (coordinator_server, _schedule_rt) = build_multi_backend_server_with_coordinator(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
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
    #[test]
    fn multi_backend_boot_paths_share_identical_wiring_surface_in_memory() {
        let khive_cfg = single_main_backend_config(khive_runtime::BackendKind::Memory, None);

        let plain_server = khive_mcp::serve::build_server_multi_backend(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
        .expect("plain multi-backend boot must succeed");

        let (coordinator_server, _schedule_rt) = build_multi_backend_server_with_coordinator(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
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

    /// #613: the two sibling tests above never configure
    /// a non-default output format, so `output_format` parity was vacuous —
    /// both paths landing on the built-in `Json` default would pass even if one
    /// path silently dropped `apply_env_output_format(khive_cfg.runtime.default_output_format)`
    /// (the exact ADR-078 regression class this consolidation exists to prevent).
    ///
    /// This case sets `[runtime].default_output_format = Table` (a non-default
    /// value, `khive_runtime::engine_config::RuntimeSectionConfig::default_output_format`)
    /// in the SAME `KhiveConfig` both constructors consume, then asserts not just
    /// that the two surfaces match but that the captured format equals the
    /// configured non-default value — the explicit expected-value check is what
    /// makes the assertion non-vacuous. `KHIVE_OUTPUT_FORMAT` is cleared and
    /// restored around the test (`#[serial]`) so an ambient env var can never
    /// mask a regression in the TOML-default resolution tier.
    #[test]
    #[serial]
    fn multi_backend_boot_paths_share_identical_non_default_output_format() {
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
        .expect("plain multi-backend boot must succeed");

        let (coordinator_server, _schedule_rt) = build_multi_backend_server_with_coordinator(
            base_multi_backend_runtime_config(),
            &khive_cfg,
            None,
        )
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

    /// The kkernel `Command::Mcp` coordinator-attached multi-backend boot path
    /// (`build_multi_backend_server_with_coordinator`, the real `kkernel mcp
    /// --daemon` production boundary) funnels through
    /// `khive_mcp::serve::build_registry_for_multi_backend` exactly like the
    /// plain `build_server_multi_backend` path does — that shared choke point
    /// is where the db-anchor consistency guard lives, so a `db_path` that
    /// diverges from the canonical anchor for the same `--db` input must be
    /// rejected here too, naming both paths.
    #[test]
    fn coordinator_boundary_rejects_diverging_db_path() {
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
        );

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

    /// Regression for #720: the coordinator-attached `kkernel mcp` path must
    /// retain the HOME-derived anchor captured during runtime-config resolution
    /// when HOME changes before registry construction.
    #[test]
    #[serial]
    fn coordinator_boot_uses_anchor_captured_by_runtime_config() {
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
        );
        if let Err(error) = result {
            panic!(
                "coordinator-attached construction must retain the anchor captured by \
                 resolve_runtime_config instead of re-reading HOME: {error}"
            );
        }
    }

    // --- #674: coordinator link-target resolution parity with `get` ---

    /// Regression for #674: a full-UUID `link(..., relation="annotates")` whose
    /// target is an edge-substrate UUID must succeed on the coordinator-attached
    /// multi-backend boot path, exactly like `get(<edge_uuid>)` does.
    ///
    /// Reproduces the production topology from the issue: two backends (`main`
    /// plus `sessions`), with the `session` pack bound to `sessions` while `kg`
    /// falls back to `main`. That pack-to-backend split is what engages the
    /// `SubstrateCoordinator` for `kg` verbs (`build_multi_backend_server_with_coordinator`,
    /// not the coordinator-less `khive_mcp::serve::build_server_multi_backend`) —
    /// a single-backend or unsplit config does not reproduce the bug.
    ///
    /// Before the fix, the coordinator's node locator only probed entity and
    /// note substrates, so `link(note, <edge_uuid>, annotates)` failed with
    /// "node <uuid> not found on any backend" even though `get(<edge_uuid>)`
    /// resolved the same UUID.
    #[tokio::test]
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
