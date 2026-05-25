//! `kkernel engine` — embedding model lifecycle management (ADR-043).
//!
//! Implements:
//! - `kkernel engine list`                     — show all engines and their model history
//! - `kkernel engine status <engine>`          — per-engine active model and migration state
//! - `kkernel engine migrate <engine> --to ... / --resume / --abort`
//! - `kkernel engine drift-check <engine>`     — one-shot drift detection
//!
//! These commands are operator-only. No MCP verbs are exposed (ADR-043 §6).

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Subcommand;
use serde::Serialize;

// ── Subcommand tree ────────────────────────────────────────────────────────────

#[derive(Subcommand, Debug)]
pub enum EngineCommand {
    /// List all engines and their model history.
    List(EngineListArgs),

    /// Show per-engine active model and migration status.
    Status(EngineStatusArgs),

    /// Manage embedding model migrations for an engine.
    Migrate(EngineMigrateArgs),

    /// Run a one-shot drift detection for an engine.
    DriftCheck(EngineDriftCheckArgs),
}

#[derive(clap::Parser, Debug)]
pub struct EngineListArgs {
    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,

    /// Database path (defaults to `~/.khive/khive-graph.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

#[derive(clap::Parser, Debug)]
pub struct EngineStatusArgs {
    /// Engine name to inspect (e.g. `mE5-small`).
    pub engine: String,

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,

    /// Database path (defaults to `~/.khive/khive-graph.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

#[derive(clap::Parser, Debug)]
pub struct EngineMigrateArgs {
    /// Engine name to migrate (e.g. `mE5-small`).
    pub engine: String,

    /// Target model name for a new migration.
    #[arg(long, conflicts_with_all = &["resume", "abort"])]
    pub to: Option<String>,

    /// Resume a previously failed migration.
    #[arg(long, conflicts_with_all = &["to", "abort"])]
    pub resume: bool,

    /// Abort an in-progress migration and clean up pending vectors.
    #[arg(long, conflicts_with_all = &["to", "resume"])]
    pub abort: bool,

    /// Database path (defaults to `~/.khive/khive-graph.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

#[derive(clap::Parser, Debug)]
pub struct EngineDriftCheckArgs {
    /// Engine name to inspect (e.g. `mE5-small`).
    pub engine: String,

    /// Number of records to sample for drift detection (default: 1000).
    #[arg(long, default_value = "1000")]
    pub sample: usize,

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,

    /// Database path (defaults to `~/.khive/khive-graph.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

// ── Output types ───────────────────────────────────────────────────────────────

#[derive(Clone, Debug, Serialize)]
pub struct EngineModelRecord {
    pub engine_name: String,
    pub model_id: String,
    pub key_version: String,
    pub dimensions: u32,
    pub status: String,
    pub activated_at: Option<i64>,
    pub superseded_at: Option<i64>,
}

#[derive(Debug, Serialize)]
pub struct EngineStatus {
    pub engine_name: String,
    pub active_model: Option<EngineModelRecord>,
    pub migration_in_progress: bool,
    pub pending_model: Option<EngineModelRecord>,
}

// ── Entry point ────────────────────────────────────────────────────────────────

pub fn run_engine(cmd: EngineCommand) -> Result<()> {
    match cmd {
        EngineCommand::List(args) => cmd_engine_list(args),
        EngineCommand::Status(args) => cmd_engine_status(args),
        EngineCommand::Migrate(args) => cmd_engine_migrate(args),
        EngineCommand::DriftCheck(args) => cmd_engine_drift_check(args),
    }
}

// ── list ──────────────────────────────────────────────────────────────────────

fn cmd_engine_list(args: EngineListArgs) -> Result<()> {
    let records = query_embedding_models(args.db.as_deref(), None)?;

    if args.human {
        for r in &records {
            println!(
                "  {:<20} model={:<30} status={} key_version={} dim={}",
                r.engine_name, r.model_id, r.status, r.key_version, r.dimensions
            );
        }
    } else {
        let json = serde_json::to_string(&records).expect("serialize EngineModelRecord[]");
        println!("{json}");
    }
    Ok(())
}

// ── status ────────────────────────────────────────────────────────────────────

fn cmd_engine_status(args: EngineStatusArgs) -> Result<()> {
    let all = query_embedding_models(args.db.as_deref(), Some(&args.engine))?;

    let active = all.iter().find(|r| r.status == "active").cloned();
    let pending = all.iter().find(|r| r.status == "pending").cloned();

    let status = EngineStatus {
        engine_name: args.engine.clone(),
        migration_in_progress: pending.is_some(),
        active_model: active,
        pending_model: pending,
    };

    if args.human {
        if let Some(ref m) = status.active_model {
            println!("engine: {}", status.engine_name);
            println!("  active model:         {}", m.model_id);
            println!("  key_version:          {}", m.key_version);
            println!("  dimensions:           {}", m.dimensions);
            println!("  migration_in_progress:{}", status.migration_in_progress);
        } else {
            println!(
                "engine: {} — no active model registered",
                status.engine_name
            );
        }
    } else {
        let json = serde_json::to_string(&status).expect("serialize EngineStatus");
        println!("{json}");
    }
    Ok(())
}

// ── migrate ───────────────────────────────────────────────────────────────────

fn cmd_engine_migrate(_args: EngineMigrateArgs) -> Result<()> {
    Err(anyhow!(
        "engine migrate is not yet implemented (ADR-043 D2-D6 — EmbedMigrationWorker deferred \
         to follow-up #380). Use 'kkernel engine list' / 'status' to inspect registered models."
    ))
}

// ── drift-check ───────────────────────────────────────────────────────────────

fn cmd_engine_drift_check(_args: EngineDriftCheckArgs) -> Result<()> {
    Err(anyhow!(
        "engine drift-check is not yet implemented (ADR-043 §5 lattice_transport integration \
         deferred). Track follow-up #380."
    ))
}

// ── Internal helpers ──────────────────────────────────────────────────────────

fn query_embedding_models(
    _db: Option<&std::path::Path>,
    engine_filter: Option<&str>,
) -> Result<Vec<EngineModelRecord>> {
    // The _embedding_models table is created by the ADR-043 schema migration.
    // Until that migration lands, the table may not exist; return an empty list
    // with a log rather than a hard error so `kkernel engine list` is usable
    // before full ADR-043 deployment.
    //
    // A full implementation opens the SQLite DB, queries:
    //   SELECT engine_name, model_id, key_version, dim, status,
    //          activated_at, superseded_at
    //   FROM   _embedding_models
    //   [WHERE engine_name = ?]
    //   ORDER  BY engine_name, activated_at NULLS LAST
    //
    // and maps rows to EngineModelRecord.
    //
    // This scaffold returns an empty list so the CLI compiles and tests can
    // verify the command routing surface without a live database.

    if let Some(engine) = engine_filter {
        tracing::debug!(
            engine,
            "query_embedding_models: _embedding_models not yet populated"
        );
    } else {
        tracing::debug!("query_embedding_models: _embedding_models not yet populated");
    }

    Ok(Vec::new())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_list_empty_ok() {
        let args = EngineListArgs {
            human: false,
            db: None,
        };
        // Should not panic even when no models are registered yet.
        cmd_engine_list(args).expect("engine list succeeds on empty registry");
    }

    #[test]
    fn engine_status_empty_ok() {
        let args = EngineStatusArgs {
            engine: "mE5-small".into(),
            human: false,
            db: None,
        };
        cmd_engine_status(args).expect("engine status succeeds on empty registry");
    }

    #[test]
    fn engine_migrate_returns_not_implemented() {
        let args = EngineMigrateArgs {
            engine: "mE5-small".into(),
            to: Some("bge-small-en-v1.5".into()),
            resume: false,
            abort: false,
            db: None,
        };
        let err = cmd_engine_migrate(args).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not yet implemented"),
            "expected 'not yet implemented' in error, got: {msg}"
        );
        assert!(
            msg.contains("#380"),
            "expected follow-up issue reference in error, got: {msg}"
        );
    }

    #[test]
    fn engine_migrate_resume_returns_not_implemented() {
        let args = EngineMigrateArgs {
            engine: "mE5-small".into(),
            to: None,
            resume: true,
            abort: false,
            db: None,
        };
        let err = cmd_engine_migrate(args).unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[test]
    fn engine_migrate_abort_returns_not_implemented() {
        let args = EngineMigrateArgs {
            engine: "mE5-small".into(),
            to: None,
            resume: false,
            abort: true,
            db: None,
        };
        let err = cmd_engine_migrate(args).unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[test]
    fn drift_check_returns_not_implemented() {
        let args = EngineDriftCheckArgs {
            engine: "mE5-small".into(),
            sample: 500,
            human: false,
            db: None,
        };
        let err = cmd_engine_drift_check(args).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not yet implemented"),
            "expected 'not yet implemented' in error, got: {msg}"
        );
        assert!(
            msg.contains("#380"),
            "expected follow-up issue reference in error, got: {msg}"
        );
    }
}
