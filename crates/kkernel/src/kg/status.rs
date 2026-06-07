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
mod tests {
    use tempfile::TempDir;

    use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig};

    use crate::kg::archive::archive_from_ndjson_repo;

    #[tokio::test]
    async fn status_hashes_clean_after_sync() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path();
        let entity_id = "ffffffff-ffff-ffff-ffff-ffffffffffff";
        let entity_ndjson = format!(
            r#"{{"id":"{entity_id}","kind":"concept","name":"StatusEntity","properties":{{}},"tags":[]}}"#
        );
        let kg_dir = repo.join(".khive/kg");
        std::fs::create_dir_all(&kg_dir).unwrap();
        std::fs::write(kg_dir.join("entities.ndjson"), &entity_ndjson).unwrap();
        std::fs::write(kg_dir.join("edges.ndjson"), "").unwrap();

        let db = repo.join(".khive/state/working.db");
        crate::sync::run_sync(repo, &db, "test-ns").await.unwrap();

        let ns = Namespace::parse("test-ns").unwrap();
        let config = RuntimeConfig {
            db_path: Some(db),
            default_namespace: ns.clone(),
            embedding_model: None,
            ..Default::default()
        };
        let runtime = KhiveRuntime::new(config).unwrap();
        let token = runtime.authorize(ns).unwrap();

        let db_archive = runtime.export_kg(&token).await.unwrap();
        let ndjson_archive = archive_from_ndjson_repo(repo, "test-ns").unwrap();

        let db_hash = khive_vcs::hash::snapshot_id_for_archive(&db_archive).unwrap();
        let ndjson_hash = khive_vcs::hash::snapshot_id_for_archive(&ndjson_archive).unwrap();
        assert_eq!(db_hash, ndjson_hash, "hashes must match after sync");
    }
}
