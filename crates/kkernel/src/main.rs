//! `kkernel` binary — khive admin/management Rust CLI.
//!
//! See [ADR-003](../../docs/adr/ADR-003-system-architecture.md) for the
//! kernel/MCP split rationale.
//!
//! Subcommands:
//!
//! - `sync`    — build a queryable SQLite DB from NDJSON sources (issue #174)
//! - `pack`    — introspect registered packs (`list`, `handler <name>`)
//! - `kg`      — KG validation, init, hook management (ADR-034, ADR-035)
//! - `engine`  — embedding model lifecycle: list/status/migrate/drift-check (ADR-043)
//! - `vector`  — vector store capabilities and orphan sweep (ADR-044)
//! - `reindex` — rebuild embedding vectors for entities and notes
//! - `backend` — inspect registered backends (`list`, `info <name>`)
//!
//! All subcommands emit JSON on stdout by default for easy piping/parsing.
//! Pass `--human` to switch to a readable table where supported.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use khive_runtime::{BackendId, KhiveRuntime, RuntimeConfig};
use kkernel::{coordinator::BackendRegistry, engine, kg, pack_introspect, reindex, sync, vector};

#[derive(Parser, Debug)]
#[command(
    name = "kkernel",
    version,
    about = "khive kernel — admin/management Rust binary (ADR-076)"
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

    /// KG validation, init, and hook management (ADR-034, ADR-035).
    #[command(subcommand)]
    Kg(kg::KgCommand),

    /// Embedding model lifecycle: list, status, migrate, drift-check (ADR-043).
    #[command(subcommand)]
    Engine(engine::EngineCommand),

    /// Vector store capabilities and orphan sweep (ADR-044).
    #[command(subcommand)]
    Vector(vector::VectorCommand),

    /// Re-embed all entities and notes using the configured embedding model.
    Reindex(reindex::ReindexArgs),

    /// Inspect registered backends (ADR-009, ADR-028).
    #[command(subcommand)]
    Backend(BackendCommand),
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

/// Backend admin commands (ADR-003 §four-invariants, ADR-009, ADR-028).
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
        Command::Kg(k) => kg::run_kg(k),
        Command::Engine(e) => engine::run_engine(e),
        Command::Vector(v) => vector::run_vector(v),
        Command::Reindex(r) => reindex::run_reindex(r).await,
        Command::Backend(b) => cmd_backend(b),
    }
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
    // Full multi-backend implementation reads khive.toml (ADR-028); this ships
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
