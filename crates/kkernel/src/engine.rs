//! `kkernel engine` — embedding model lifecycle management.
//!
//! Shipped:
//! - `kkernel engine list`   — show all engines and their model history
//! - `kkernel engine status` — per-engine active model and migration state
//!
//! Deferred (returns `NotImplemented`, tracked in #380):
//! - `kkernel engine migrate`     — model migration (EmbedMigrationWorker)
//! - `kkernel engine drift-check` — one-shot drift detection (lattice_transport)
//!
//! These commands are operator-only. No MCP verbs are exposed.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Subcommand;
use serde::Serialize;

use khive_runtime::{KhiveRuntime, RuntimeConfig};

// ── Subcommand tree ────────────────────────────────────────────────────────────

/// Subcommands for `kkernel engine` — embedding model lifecycle management.
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

/// CLI arguments for `kkernel engine list`.
#[derive(clap::Parser, Debug)]
pub struct EngineListArgs {
    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,

    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

/// CLI arguments for `kkernel engine status`.
#[derive(clap::Parser, Debug)]
pub struct EngineStatusArgs {
    /// Engine name to inspect (e.g. `mE5-small`).
    pub engine: String,

    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,

    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

/// CLI arguments for `kkernel engine migrate`.
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

    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

/// CLI arguments for `kkernel engine drift-check`.
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

    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

// ── Output types ───────────────────────────────────────────────────────────────

/// A single row from the `_embedding_models` table.
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

/// Active status for a single embedding engine, including any in-progress migration.
#[derive(Debug, Serialize)]
pub struct EngineStatus {
    pub engine_name: String,
    pub active_model: Option<EngineModelRecord>,
    pub migration_in_progress: bool,
    pub pending_model: Option<EngineModelRecord>,
}

// ── Entry point ────────────────────────────────────────────────────────────────

/// Dispatch `kkernel engine` subcommands to their implementations.
pub async fn run_engine(cmd: EngineCommand) -> Result<()> {
    match cmd {
        EngineCommand::List(args) => cmd_engine_list(args).await,
        EngineCommand::Status(args) => cmd_engine_status(args).await,
        EngineCommand::Migrate(args) => cmd_engine_migrate(args),
        EngineCommand::DriftCheck(args) => cmd_engine_drift_check(args),
    }
}

// ── list ──────────────────────────────────────────────────────────────────────

async fn cmd_engine_list(args: EngineListArgs) -> Result<()> {
    let records = fetch_model_records(args.db.as_deref(), None).await?;

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

async fn cmd_engine_status(args: EngineStatusArgs) -> Result<()> {
    let all = fetch_model_records(args.db.as_deref(), Some(&args.engine)).await?;

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
        "engine migrate is not yet implemented (EmbedMigrationWorker deferred \
         to follow-up #380). Use 'kkernel engine list' / 'status' to inspect registered models."
    ))
}

// ── drift-check ───────────────────────────────────────────────────────────────

fn cmd_engine_drift_check(_args: EngineDriftCheckArgs) -> Result<()> {
    Err(anyhow!(
        "engine drift-check is not yet implemented (lattice_transport integration \
         deferred). Track follow-up #380."
    ))
}

// ── Internal helpers ──────────────────────────────────────────────────────────

/// Query `_embedding_models` via `KhiveRuntime::list_embedding_models`.
///
/// When `db` is `None` the default path (`~/.khive/khive.db`) is used.
/// If the default file does not yet exist, returns an empty vec without creating
/// it — preserving the pre-existing behaviour of `khive_db::query_embedding_models`.
async fn fetch_model_records(
    db: Option<&std::path::Path>,
    engine_filter: Option<&str>,
) -> Result<Vec<EngineModelRecord>> {
    let db_path: Option<PathBuf> = match db {
        Some(p) if p.exists() => Some(p.to_path_buf()),
        Some(p) => anyhow::bail!("database file does not exist: {}", p.display()),
        None => {
            let default = std::env::var("HOME")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
                .join(".khive/khive.db");
            if default.exists() {
                Some(default)
            } else {
                return Ok(Vec::new());
            }
        }
    };

    let cfg = RuntimeConfig {
        db_path,
        ..RuntimeConfig::default()
    };
    let rt = KhiveRuntime::new_readonly(cfg).map_err(|e| anyhow!("{e}"))?;
    let raw = rt
        .list_embedding_models(engine_filter)
        .await
        .map_err(|e| anyhow!("{e}"))?;

    Ok(raw
        .into_iter()
        .map(|r| EngineModelRecord {
            engine_name: r.engine_name,
            model_id: r.model_id,
            key_version: r.key_version,
            dimensions: r.dimensions,
            status: r.status,
            activated_at: r.activated_at,
            superseded_at: r.superseded_at,
        })
        .collect())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    // Engine tests must be hermetic: with `db: None` the command resolves to the
    // operator's real `~/.khive/khive.db` and runs migration writes against
    // it. Always point tests at a throwaway temp DB (review #531). Keep the
    // returned TempDir alive for the test's duration.
    fn temp_db() -> (TempDir, Option<std::path::PathBuf>) {
        let tmp = TempDir::new().expect("temp dir");
        let path = tmp.path().join("engine_test.db");
        std::fs::File::create(&path).expect("create empty db file");
        (tmp, Some(path))
    }

    #[tokio::test]
    async fn engine_list_empty_ok() {
        let (_tmp, db) = temp_db();
        let args = EngineListArgs { human: false, db };
        // Should not panic even when no models are registered yet.
        cmd_engine_list(args)
            .await
            .expect("engine list succeeds on empty registry");
    }

    #[tokio::test]
    async fn engine_status_empty_ok() {
        let (_tmp, db) = temp_db();
        let args = EngineStatusArgs {
            engine: "mE5-small".into(),
            human: false,
            db,
        };
        cmd_engine_status(args)
            .await
            .expect("engine status succeeds on empty registry");
    }

    #[test]
    fn engine_migrate_returns_not_implemented() {
        let (_tmp, db) = temp_db();
        let args = EngineMigrateArgs {
            engine: "mE5-small".into(),
            to: Some("bge-small-en-v1.5".into()),
            resume: false,
            abort: false,
            db,
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
        let (_tmp, db) = temp_db();
        let args = EngineMigrateArgs {
            engine: "mE5-small".into(),
            to: None,
            resume: true,
            abort: false,
            db,
        };
        let err = cmd_engine_migrate(args).unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[test]
    fn engine_migrate_abort_returns_not_implemented() {
        let (_tmp, db) = temp_db();
        let args = EngineMigrateArgs {
            engine: "mE5-small".into(),
            to: None,
            resume: false,
            abort: true,
            db,
        };
        let err = cmd_engine_migrate(args).unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }

    #[test]
    fn drift_check_returns_not_implemented() {
        let (_tmp, db) = temp_db();
        let args = EngineDriftCheckArgs {
            engine: "mE5-small".into(),
            sample: 500,
            human: false,
            db,
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
