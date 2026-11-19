use std::path::PathBuf;
use std::sync::Arc;

use anyhow::ensure;
use khive_runtime::{KhiveRuntime, Namespace, RuntimeConfig, StorageBackend};
use khive_storage::Entity;
use tempfile::TempDir;
use uuid::Uuid;

use super::PROTOCOL_DEADLINE;

pub(super) struct Fixture {
    pub root: PathBuf,
    dir: TempDir,
}

impl Fixture {
    pub async fn new() -> anyhow::Result<Self> {
        let dir = tempfile::Builder::new()
            .prefix("kh-mcp-startup-")
            .tempdir()?;
        let root = dir.path().canonicalize()?;
        // The data store is outside the helper HOME; no live config/store is read.
        for name in [
            "store",
            "home",
            "work",
            "config",
            "cache",
            "tmp",
            "volume-locks",
        ] {
            std::fs::create_dir(root.join(name))?;
        }
        std::fs::write(root.join("config.toml"), "")?;
        let db = root.join("store/kg.db");
        let backend = Arc::new(StorageBackend::sqlite_for_test_with_journal_mode_in(
            &db,
            true,
            PROTOCOL_DEADLINE,
            root.join("volume-locks"),
        )?);
        backend.prepare_core_schema()?;
        ensure!(
            backend.pool().write_queue_active(),
            "fixture writer must be queued"
        );
        let runtime = KhiveRuntime::from_prepared_backend(
            backend.clone(),
            RuntimeConfig {
                db_path: Some(db),
                volume_lock_dir: Some(root.join("volume-locks")),
                actor_id: Some("benchmark:mcp-startup".into()),
                packs: vec!["kg".into()],
                ..RuntimeConfig::no_embeddings()
            },
        )?;
        let token = runtime.authorize(Namespace::local())?;
        let entities = (0..100)
            .map(|index| {
                Entity::minimal(
                    Uuid::from_u128((5655_u128 << 64) | (index + 1)),
                    "local",
                    "concept",
                    format!("startup entity {index}"),
                    1_700_000_000_000_000,
                    1_700_000_000_000_000,
                )
            })
            .collect();
        let summary = runtime.entities(&token)?.upsert_entities(entities).await?;
        // Settle all parent ownership before a child can open this store.
        drop(runtime);
        let writer = backend.pool().take_writer_task_join();
        drop(backend);
        tokio::time::timeout(PROTOCOL_DEADLINE, writer.expect("queued writer join")).await??;
        ensure!(
            summary.attempted == 100 && summary.affected == 100 && summary.failed == 0,
            "fixture writes: {summary:?}"
        );
        Ok(Self { root, dir })
    }

    pub fn close(self) -> anyhow::Result<()> {
        self.dir.close()?;
        Ok(())
    }
}
