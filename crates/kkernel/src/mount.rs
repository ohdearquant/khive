//! Operator-only management of pinned tool-source definitions.
use anyhow::{Context, Result};
use clap::Subcommand;
use khive_mcp::serve::{
    build_registry_for_multi_backend_with_db_anchor, resolve_runtime_config_with_db_anchor,
    RuntimeConfigInputs,
};
use khive_runtime::{KhiveConfig, Namespace, PackRegistry};
use std::path::PathBuf;

#[derive(Debug, Subcommand)]
pub enum MountCommand {
    /// Replace a configured source's pinned catalog and audit the change.
    Repin {
        name: String,
        #[arg(long, env = "KHIVE_CONFIG")]
        config: Option<PathBuf>,
        #[arg(long, env = "KHIVE_DB")]
        db: Option<String>,
    },
}

pub async fn run(command: MountCommand) -> Result<()> {
    let MountCommand::Repin { name, config, db } = command;
    let (mut runtime, anchor) = resolve_runtime_config_with_db_anchor(RuntimeConfigInputs {
        db: db.as_deref(),
        config: config.as_deref(),
        namespace: Namespace::local(),
        namespace_explicit: false,
        actor_explicit: false,
        no_embed: true,
        packs: None,
        brain_profile: None,
    })?;
    let source = runtime
        .mounts
        .iter()
        .find(|mount| mount.name == name)
        .cloned()
        .context("mount is not configured")?;
    anyhow::ensure!(
        !PackRegistry::discovered_names().contains(&name.as_str()),
        "mount namespace collides with a native pack"
    );
    let operator = runtime
        .actor_id
        .clone()
        .unwrap_or_else(|| "operator".into());
    let cfg = KhiveConfig::load_with_home_fallback(
        config.as_deref(),
        khive_mcp::serve::config_discovery_db_anchor(db.as_deref()).as_deref(),
    )?
    .unwrap_or_default();
    // Prepare the configured storage topology without starting unrelated sources.
    runtime.mounts.clear();
    let built = build_registry_for_multi_backend_with_db_anchor(
        runtime,
        &cfg,
        db.as_deref(),
        anchor.as_deref(),
    )
    .await?;
    let mount = khive_mounts::MountedPack::start(source, built.default_runtime).await?;
    println!("{}", serde_json::to_string(&mount.repin(&operator).await?)?);
    Ok(())
}
