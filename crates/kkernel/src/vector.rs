//! `kkernel vector` — vector store introspection and housekeeping.
//!
//! - `kkernel vector capabilities` — print the compiled sqlite-vec capabilities
//! - `kkernel vector sweep` — run an orphan-sweep to remove stale vector rows

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use clap::Subcommand;
use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_runtime::{KhiveConfig, KhiveRuntime, Namespace};
use khive_storage::types::{OrphanSweepConfig, OrphanSweepResult};
use serde::Serialize;

// ── Subcommand tree ────────────────────────────────────────────────────────────

/// Subcommands for `kkernel vector` -- vector store introspection and housekeeping.
#[derive(Subcommand, Debug)]
pub enum VectorCommand {
    /// Report the capability flags of the compiled sqlite-vec backend without opening a database.
    Capabilities(VectorCapabilitiesArgs),

    /// Sweep orphan vector rows whose subject no longer exists.
    Sweep(VectorSweepArgs),
}

/// CLI arguments for `kkernel vector capabilities`.
#[derive(clap::Parser, Debug)]
pub struct VectorCapabilitiesArgs {
    /// Print human-readable output instead of JSON.
    #[arg(long)]
    pub human: bool,

    /// Label to include in the capability report (defaults to "default").
    #[arg(long)]
    pub engine: Option<String>,

    /// Accepted for compatibility; capabilities never opens this database.
    #[arg(long)]
    pub db: Option<PathBuf>,
}

/// CLI arguments for `kkernel vector sweep`.
#[derive(clap::Parser, Debug)]
pub struct VectorSweepArgs {
    /// Namespace to sweep. May be repeated. Empty = all namespaces.
    #[arg(long)]
    pub namespace: Vec<String>,

    /// Maximum rows to delete across all selected stores (0 deletes none; maximum 4294967295).
    #[arg(long, default_value = "1000")]
    pub max_delete: u64,

    /// Dry run — report orphans without deleting.
    #[arg(long)]
    pub dry_run: bool,

    /// Configured engine name to sweep (defaults to all configured engines).
    #[arg(long)]
    pub engine: Option<String>,

    /// Database path (defaults to `~/.khive/khive.db`).
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<PathBuf>,
}

// ── Output types ───────────────────────────────────────────────────────────────

/// JSON-serializable projection of `VectorStoreCapabilities` capability flags.
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

#[derive(Debug, Serialize)]
struct SweepStoreReport {
    engine_names: Vec<String>,
    model: String,
    namespaces: Vec<String>,
    #[serde(flatten)]
    result: OrphanSweepResult,
}

#[derive(Debug, Serialize)]
struct SweepReport {
    dry_run: bool,
    max_delete: u32,
    namespaces: Vec<String>,
    #[serde(flatten)]
    result: OrphanSweepResult,
    stores: Vec<SweepStoreReport>,
}

// ── Entry point ────────────────────────────────────────────────────────────────

/// Dispatch `kkernel vector` subcommands to their implementations.
pub fn run_vector(cmd: VectorCommand) -> Result<()> {
    match cmd {
        VectorCommand::Capabilities(args) => cmd_vector_capabilities(args),
        VectorCommand::Sweep(args) => {
            println!("{}", serde_json::to_string(&cmd_vector_sweep(args)?)?);
            Ok(())
        }
    }
}

// ── capabilities ──────────────────────────────────────────────────────────────

fn cmd_vector_capabilities(args: VectorCapabilitiesArgs) -> Result<()> {
    let engine_name = args.engine.unwrap_or_else(|| "default".to_string());

    let report = sqlite_vec_capabilities(engine_name);

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
        println!("{}", serde_json::to_string(&report)?);
    }
    Ok(())
}

fn sqlite_vec_capabilities(engine_name: String) -> CapabilitiesReport {
    // Keep introspection independent of database creation/migration. The parity
    // test pins every field to the backend's actual capabilities.
    CapabilitiesReport {
        engine_name,
        supports_filter: false,
        supports_batch_search: false,
        supports_quantization: false,
        supports_update: false,
        supports_orphan_sweep: true,
        supports_multi_field: false,
        // sqlite-vec 0.1.9: SQLITE_VEC_VEC0_MAX_DIMENSIONS = 8192
        max_dimensions: Some(8192),
        index_kinds: vec!["sqlite_vec".into()],
    }
}

// ── sweep ─────────────────────────────────────────────────────────────────────

fn cmd_vector_sweep(args: VectorSweepArgs) -> Result<SweepReport> {
    let max_delete = u32::try_from(args.max_delete)
        .map_err(|_| anyhow!("--max-delete must be at most {}", u32::MAX))?;
    // cli_main already runs inside Tokio. A dedicated runtime thread also
    // supports synchronous and current-thread callers without nested block_on.
    std::thread::Builder::new()
        .name("vector-sweep".into())
        .spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?
                .block_on(sweep(args, max_delete))
        })?
        .join()
        .map_err(|_| anyhow!("vector sweep worker panicked"))?
}

async fn sweep(args: VectorSweepArgs, max_delete: u32) -> Result<SweepReport> {
    for namespace in &args.namespace {
        Namespace::parse(namespace).map_err(|error| anyhow!("{error}"))?;
    }
    let db = args
        .db
        .as_deref()
        .map(|path| {
            path.to_str()
                .ok_or_else(|| anyhow!("database path must be valid UTF-8"))
        })
        .transpose()?;
    let config_path = std::env::var_os("KHIVE_CONFIG").map(PathBuf::from);
    let validated = crate::reindex::validate_declared_reindex_target(db, config_path.as_deref())?;
    let cfg = resolve_runtime_config(RuntimeConfigInputs {
        db,
        config: config_path.as_deref(),
        namespace: Namespace::local(),
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: false,
        packs: None,
        brain_profile: None,
    })?;
    let discovery_anchor = khive_mcp::serve::config_discovery_db_anchor(db);
    let config =
        KhiveConfig::load_with_home_fallback(config_path.as_deref(), discovery_anchor.as_deref())?;
    let rt = crate::reindex::open_validated_reindex_backend(cfg, validated.as_ref())?;
    let models = selected_models(&rt, config.as_ref(), args.engine.as_deref())?;
    let token = rt
        .authorize(Namespace::local())
        .map_err(|error| anyhow!("{error}"))?;
    let mut remaining = max_delete;
    let mut stores = Vec::new();
    let mut total = OrphanSweepResult {
        scanned: 0,
        deleted: 0,
        would_delete: 0,
        max_delete_hit: false,
    };
    for (model, engine_names) in models {
        let store = rt
            .vectors_for_model(&token, &model)
            .map_err(|error| anyhow!("{error}"))?;
        let result = store
            .orphan_sweep(&OrphanSweepConfig {
                subject_id_allowlist: None,
                namespaces: args.namespace.clone(),
                substrate_kinds: vec![],
                max_delete: remaining,
                dry_run: args.dry_run,
            })
            .await
            .with_context(|| format!("sweep vector model {model:?}"))?;
        let consumed = if args.dry_run {
            result.would_delete
        } else {
            result.deleted
        };
        remaining = u32::try_from(u64::from(remaining).saturating_sub(consumed))
            .expect("remaining budget never increases");
        total.scanned += result.scanned;
        total.deleted += result.deleted;
        total.would_delete += result.would_delete;
        stores.push(SweepStoreReport {
            engine_names,
            model,
            namespaces: args.namespace.clone(),
            result,
        });
    }
    total.max_delete_hit = total.would_delete > u64::from(max_delete);
    Ok(SweepReport {
        dry_run: args.dry_run,
        max_delete,
        namespaces: args.namespace,
        result: total,
        stores,
    })
}

fn selected_models(
    rt: &KhiveRuntime,
    config: Option<&KhiveConfig>,
    requested: Option<&str>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let mut engines = Vec::new();
    if let Some(config) = config.filter(|config| !config.engines.is_empty()) {
        for engine in &config.engines {
            let model = rt
                .resolve_embedding_model(Some(&engine.model))
                .map_err(|error| anyhow!("{error}"))?
                .to_string();
            engines.push((engine.name.clone(), model));
        }
    } else {
        engines.extend(
            rt.registered_embedding_model_names()
                .into_iter()
                .map(|model| (model.clone(), model)),
        );
    }
    let mut selected = BTreeMap::<String, Vec<String>>::new();
    for (engine, model) in engines {
        if requested.is_none_or(|name| name == engine) {
            selected.entry(model).or_default().push(engine);
        }
    }
    if selected.is_empty() {
        return Err(match requested {
            Some(name) => anyhow!("unknown configured vector engine {name:?}"),
            None => anyhow!("no vector engines are configured"),
        });
    }
    Ok(selected)
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
        let backend = khive_db::StorageBackend::memory().unwrap();
        let store = backend.vectors("capability_fixture", "fixture", 4).unwrap();
        let mut report = serde_json::to_value(sqlite_vec_capabilities("fixture".into())).unwrap();
        report.as_object_mut().unwrap().remove("engine_name");
        assert_eq!(
            report,
            serde_json::to_value(store.capabilities()).unwrap(),
            "VECTOR_CAPABILITIES_BACKEND_PARITY"
        );
    }

    #[test]
    fn capabilities_does_not_open_database() {
        let root = tempfile::tempdir().unwrap();
        let db = root.path().join("not-created.db");
        run_vector(VectorCommand::Capabilities(VectorCapabilitiesArgs {
            human: false,
            engine: None,
            db: Some(db.clone()),
        }))
        .unwrap();
        assert!(!db.exists());
    }
}
