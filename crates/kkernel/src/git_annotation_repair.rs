//! One-shot operator preview/apply for historical commit-to-project annotations.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use uuid::Uuid;

use khive_mcp::serve::{resolve_runtime_config, RuntimeConfigInputs};
use khive_pack_git::reconcile::{run_reconciliation, ReconcileOptions};
use khive_runtime::{IngestAuditStore, KhiveRuntime, Namespace, PackRegistry};

#[derive(Parser, Debug)]
pub struct GitAnnotationRepairArgs {
    /// Canonical local repository source containing a .git entry.
    #[arg(long)]
    pub repo: PathBuf,
    /// Exact live project UUID; prefixes are not accepted.
    #[arg(long)]
    pub project: Uuid,
    /// Full frozen commit object ID on the selected source's HEAD history.
    #[arg(long)]
    pub frozen_tip: String,
    /// Apply only the preview with this exact ID; omit for read-only preview.
    #[arg(long)]
    pub apply_preview: Option<String>,
    #[arg(long, env = "KHIVE_DB")]
    pub db: Option<String>,
    #[arg(long, default_value = "local")]
    pub namespace: String,
}

pub async fn run(args: GitAnnotationRepairArgs) -> Result<()> {
    let namespace =
        Namespace::parse(&args.namespace).map_err(|error| anyhow::anyhow!("{error}"))?;
    let config = resolve_runtime_config(RuntimeConfigInputs {
        db: args.db.as_deref(),
        config: None,
        namespace,
        namespace_explicit: true,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })?;
    let runtime = KhiveRuntime::new(config).map_err(|error| anyhow::anyhow!("{error}"))?;
    let token = runtime
        .authorize(runtime.config().default_namespace.clone())
        .map_err(|error| anyhow::anyhow!("{error}"))
        .context("authorizing namespace")?;
    let _registry = PackRegistry::build_ingest_registry(&runtime, IngestAuditStore::Detach)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    let report = run_reconciliation(
        &runtime,
        &token,
        ReconcileOptions {
            repo: args.repo,
            project_id: args.project,
            frozen_tip: args.frozen_tip,
            apply_preview_id: args.apply_preview,
        },
    )
    .await?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    if report.apply.is_some() && !report.success {
        anyhow::bail!("historical annotation apply did not complete");
    }
    Ok(())
}
