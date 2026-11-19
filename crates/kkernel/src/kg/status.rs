//! `kkernel kg status` — compare DB state against on-disk NDJSON files.

use anyhow::{Context, Result};
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};

use super::archive::archive_from_ndjson_repo;
use super::types::{KgStatusReport, StatusArgs};

pub(super) async fn cmd_status(args: StatusArgs) -> Result<()> {
    let ns = Namespace::parse(&args.namespace)?;
    let config = RuntimeConfig {
        db_path: Some(args.db.clone()),
        default_namespace: ns.clone(),
        embedding_model: None,
        ..Default::default()
    };
    let runtime = KhiveRuntime::new(config)?;
    let token = runtime.authorize(ns)?;

    let db_archive = runtime.export_kg(&token).await?;
    let db_hash = khive_vcs::hash::snapshot_id_for_archive(&db_archive)
        .context("hash DB archive")?
        .as_str()
        .to_string();

    let ndjson_archive = archive_from_ndjson_repo(&args.repo, &args.namespace)?;
    let ndjson_hash = khive_vcs::hash::snapshot_id_for_archive(&ndjson_archive)
        .context("hash NDJSON archive")?
        .as_str()
        .to_string();

    let report = KgStatusReport {
        clean: db_hash == ndjson_hash,
        db_hash,
        ndjson_hash,
    };
    let json = serde_json::to_string(&report).expect("serialize KgStatusReport");
    println!("{json}");
    Ok(())
}

#[cfg(test)]
#[path = "status_tests.rs"]
mod tests;
