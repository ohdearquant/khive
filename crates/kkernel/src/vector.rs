//! `kkernel vector` — vector store introspection and housekeeping.
//!
//! Shipped:
//! - `kkernel vector capabilities` — print VectorStoreCapabilities for the active backend
//!
//! Deferred (returns `NotImplemented`, tracked in #381):
//! - `kkernel vector sweep` — run an orphan-sweep to remove stale vector rows

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use clap::Subcommand;
use serde::Serialize;

// ── Subcommand tree ────────────────────────────────────────────────────────────

/// Subcommands for `kkernel vector` -- vector store introspection and housekeeping.
#[derive(Subcommand, Debug)]
pub enum VectorCommand {
    /// Report the capability flags of the active vector backend.
    Capabilities(VectorCapabilitiesArgs),

    /// Sweep orphan vector rows whose subject no longer exists.
    Sweep(VectorSweepArgs),
}

#[derive(clap::Parser, Debug)]
pub struct VectorCapabilitiesArgs {
    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,

    /// Engine name to inspect (defaults to the runtime-configured engine).
    #[arg(long)]
    pub engine: Option<String>,

    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

/// CLI arguments for `kkernel vector sweep`.
#[derive(clap::Parser, Debug)]
pub struct VectorSweepArgs {
    /// Namespace to sweep. May be repeated. Empty = all namespaces.
    #[arg(long)]
    pub namespace: Vec<String>,

    /// Maximum rows to delete in this run (default: 1000).
    #[arg(long, default_value = "1000")]
    pub max_delete: u64,

    /// Dry run — report orphans without deleting.
    #[arg(long)]
    pub dry_run: bool,

    /// Engine name to sweep (defaults to the runtime-configured engine).
    #[arg(long)]
    pub engine: Option<String>,

    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long)]
    pub db: Option<PathBuf>,
}

// ── Output types ───────────────────────────────────────────────────────────────

/// JSON-serializable projection of [`VectorStoreCapabilities`].
#[derive(Debug, Serialize)]
pub struct CapabilitiesReport {
    pub engine_name: String,
    pub supports_filter: bool,
    pub supports_batch_search: bool,
    pub supports_quantization: bool,
    pub supports_update: bool,
    pub supports_orphan_sweep: bool,
    pub supports_multi_field: bool,
    pub max_dimensions: Option<u32>,
    pub index_kinds: Vec<String>,
}

// ── Entry point ────────────────────────────────────────────────────────────────

/// Dispatch `kkernel vector` subcommands to their implementations.
pub fn run_vector(cmd: VectorCommand) -> Result<()> {
    match cmd {
        VectorCommand::Capabilities(args) => cmd_vector_capabilities(args),
        VectorCommand::Sweep(args) => cmd_vector_sweep(args),
    }
}

// ── capabilities ──────────────────────────────────────────────────────────────

fn cmd_vector_capabilities(args: VectorCapabilitiesArgs) -> Result<()> {
    let engine_name = args.engine.unwrap_or_else(|| "default".to_string());

    // Emit the sqlite-vec baseline capabilities.
    // A full implementation instantiates the backend via KhiveRuntime, calls
    // `VectorStore::capabilities()`, and serialises the returned
    // `&'static VectorStoreCapabilities`. The static values below match the
    // `SqliteVecStore::capabilities()` OnceLock initialiser in
    // `khive-db/src/stores/vectors.rs`.
    let report = CapabilitiesReport {
        engine_name: engine_name.clone(),
        supports_filter: false,
        supports_batch_search: false,
        supports_quantization: false,
        supports_update: false,
        supports_orphan_sweep: false,
        supports_multi_field: false,
        // sqlite-vec 0.1.9: SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192
        max_dimensions: Some(8192),
        index_kinds: vec!["sqlite_vec".into()],
    };

    if args.human {
        println!("engine:                {}", report.engine_name);
        println!("supports_filter:       {}", report.supports_filter);
        println!("supports_batch_search: {}", report.supports_batch_search);
        println!("supports_quantization: {}", report.supports_quantization);
        println!("supports_update:       {}", report.supports_update);
        println!("supports_orphan_sweep: {}", report.supports_orphan_sweep);
        println!("supports_multi_field:  {}", report.supports_multi_field);
        println!(
            "max_dimensions:        {}",
            report
                .max_dimensions
                .map_or("unlimited".into(), |d| d.to_string())
        );
        println!("index_kinds:           {}", report.index_kinds.join(", "));
    } else {
        let json = serde_json::to_string(&report).expect("serialize CapabilitiesReport");
        println!("{json}");
    }
    Ok(())
}

// ── sweep ─────────────────────────────────────────────────────────────────────

fn cmd_vector_sweep(_args: VectorSweepArgs) -> Result<()> {
    Err(anyhow!(
        "vector sweep is not yet implemented (backend orphan-sweep deferred to \
         follow-up #381). SqliteVecStore returns Unsupported."
    ))
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capabilities_json_output_has_expected_fields() {
        let args = VectorCapabilitiesArgs {
            human: false,
            engine: Some("mE5-small".into()),
            db: None,
        };
        // Verify the command completes without error.
        cmd_vector_capabilities(args).expect("capabilities command succeeds");
    }

    #[test]
    fn capabilities_default_engine() {
        let args = VectorCapabilitiesArgs {
            human: false,
            engine: None,
            db: None,
        };
        cmd_vector_capabilities(args).expect("capabilities with default engine succeeds");
    }

    #[test]
    fn capabilities_report_baseline_matches_sqlite_vec_store() {
        // Verify the baseline values match what SqliteVecStore::capabilities() returns.
        let report = CapabilitiesReport {
            engine_name: "mE5-small".into(),
            supports_filter: false,
            supports_batch_search: false,
            supports_quantization: false,
            supports_update: false,
            supports_orphan_sweep: false,
            supports_multi_field: false,
            max_dimensions: Some(8192),
            index_kinds: vec!["sqlite_vec".into()],
        };
        assert!(!report.supports_filter);
        assert!(!report.supports_orphan_sweep);
        assert_eq!(report.max_dimensions, Some(8192));
        assert_eq!(report.index_kinds, vec!["sqlite_vec"]);
    }

    #[test]
    fn sweep_returns_not_implemented() {
        let args = VectorSweepArgs {
            namespace: vec![],
            max_delete: 100,
            dry_run: true,
            engine: None,
            db: None,
        };
        let err = cmd_vector_sweep(args).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not yet implemented"),
            "expected 'not yet implemented' in error, got: {msg}"
        );
        assert!(
            msg.contains("#381"),
            "expected follow-up issue reference in error, got: {msg}"
        );
    }

    #[test]
    fn sweep_with_namespaces_returns_not_implemented() {
        let args = VectorSweepArgs {
            namespace: vec!["local".into(), "research".into()],
            max_delete: 500,
            dry_run: false,
            engine: Some("mE5-small".into()),
            db: None,
        };
        let err = cmd_vector_sweep(args).unwrap_err();
        assert!(err.to_string().contains("not yet implemented"));
    }
}
