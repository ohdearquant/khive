//! `kkernel` binary — khive admin/management Rust CLI.
//!
//! See [ADR-076](../../docs/adr/ADR-076-kkernel-and-mcp-split.md) for the
//! kernel/MCP split rationale.
//!
//! Subcommands:
//!
//! - `sync`   — build a queryable SQLite DB from NDJSON sources (issue #174)
//! - `pack`   — introspect registered packs (`list`, `handler <name>`)
//! - `kg`     — KG validation, init, hook management (ADR-034, ADR-035)
//! - `engine` — embedding model lifecycle: list/status/migrate/drift-check (ADR-043)
//! - `vector` — vector store capabilities and orphan sweep (ADR-044)
//!
//! All subcommands emit JSON on stdout by default for easy piping/parsing.
//! Pass `--human` to switch to a readable table where supported.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};

use kkernel::{engine, kg, pack_introspect, sync, vector};

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
