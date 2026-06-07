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
//! - `reindex` — rebuild embedding vectors for entities and notes
//! - `exec`    — run a verb DSL expression through the pack registry
//! - `mcp`     — serve the MCP `request` surface (stdio / daemon / transports)
//! - `backend` — inspect registered backends (`list`, `info <name>`)
//!
//! All subcommands emit JSON on stdout by default for easy piping/parsing.
//! Pass `--human` to switch to a readable table where supported.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use khive_runtime::{BackendId, KhiveRuntime, RuntimeConfig};
use kkernel::{
    coordinator::BackendRegistry, engine, exec, kg, pack_introspect, reindex, sync, vector,
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

    #[command(subcommand)]
    command: Command,
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

    /// Re-embed all entities and notes using the configured embedding model.
    Reindex(reindex::ReindexArgs),

    /// Execute a verb DSL expression (same syntax as MCP `request` tool).
    Exec(exec::ExecArgs),

    /// Serve the MCP `request` surface (stdio by default; `--daemon` for the
    /// warm Unix-socket server; `--transport` selects a registered transport).
    Mcp(khive_mcp::args::Args),

    /// Inspect registered backends.
    #[command(subcommand)]
    Backend(BackendCommand),
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

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    init_tracing(&args.log);

    match args.command {
        Command::Sync(s) => cmd_sync(s).await,
        Command::Pack(p) => cmd_pack(p),
        Command::Kg(k) => kg::run_kg(k).await,
        Command::Db(d) => cmd_db(d).await,
        Command::Engine(e) => engine::run_engine(e).await,
        Command::Vector(v) => vector::run_vector(v),
        Command::Reindex(r) => reindex::run_reindex(r).await,
        Command::Exec(e) => exec::run_exec(e).await,
        Command::Mcp(a) => {
            khive_mcp::serve::run(a, &khive_mcp::transport::TransportRegistry::with_builtins())
                .await
        }
        Command::Backend(b) => cmd_backend(b),
    }
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
    // Tracing goes to stderr — stdout is reserved for JSON results.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(level)
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
    use tempfile::TempDir;

    // A schema check must be read-only: it must not create a missing database,
    // and it must not migrate (mutate) an existing one. Regression for the codex
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
}
