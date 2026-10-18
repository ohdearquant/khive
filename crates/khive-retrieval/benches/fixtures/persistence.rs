use std::path::{Path, PathBuf};
use std::time::Duration;

use khive_db::StorageBackend;
use khive_retrieval::{Bm25Index, HnswConfig, HnswIndex, NodeId};
use uuid::Uuid;

pub const ROWS: usize = 500;
pub const DIMENSIONS: usize = 384;

struct ScratchDir(Option<PathBuf>);

impl ScratchDir {
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!("khive-retrieval-bench-{}", Uuid::new_v4()));
        std::fs::create_dir(&path).expect("create private fixture directory");
        Self(Some(path))
    }

    fn path(&self) -> &Path {
        self.0.as_deref().expect("open directory")
    }

    fn close(mut self) {
        let path = self.0.take().expect("open directory");
        std::fs::remove_dir_all(path).expect("remove benchmark database and private locks");
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        // Best-effort panic cleanup; normal cleanup uses close and fails loudly.
        if let Some(path) = self.0.take() {
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

pub struct Fixture {
    pub backend: StorageBackend,
    dir: ScratchDir,
}

impl Fixture {
    /// Called inside the shared runtime, before timing. The test-support constructor
    /// uses WAL, two readers, and an explicit private volume-lock directory.
    pub fn new() -> Self {
        let dir = ScratchDir::new();
        let backend = StorageBackend::sqlite_for_test_with_journal_mode_in(
            dir.path().join("snapshots.db"),
            true,
            Duration::from_secs(10),
            dir.path().join("volume-locks"),
        )
        .expect("open queued WAL fixture");
        backend
            .prepare_core_schema()
            .expect("prepare migrated core schema");
        assert!(backend.pool().write_queue_active());
        assert!(backend
            .pool()
            .writer_task_handle()
            .expect("writer task initialization")
            .is_some());
        Self { backend, dir }
    }

    /// Every persistence/SQL owner must be dropped by the caller first. Completion
    /// includes the writer's close-time checkpoint, before deleting its directory.
    pub async fn close(self) {
        let Self { backend, dir } = self;
        let writer = backend
            .pool()
            .take_writer_task_join()
            .expect("fixture owns writer join");
        drop(backend);
        tokio::time::timeout(Duration::from_secs(30), writer)
            .await
            .expect("writer teardown deadline")
            .expect("writer joined");
        dir.close();
    }
}

pub struct Indexes {
    pub hnsw: HnswIndex,
    pub bm25: Bm25Index,
    pub hnsw_value: serde_json::Value,
    pub bm25_value: serde_json::Value,
}

impl Indexes {
    /// Fixed seed, fixed insertion order and deterministic IDs/text. The distinct
    /// old corpus is also 500 items, so replacement cannot degenerate to a replay.
    pub fn generated(seed: u64, id_base: usize) -> Self {
        let mut hnsw =
            HnswIndex::with_config(HnswConfig::with_dimensions(DIMENSIONS).with_seed(seed));
        let mut bm25 = Bm25Index::default();
        let mut state = seed;
        for index in 0..ROWS {
            let vector = (0..DIMENSIONS)
                .map(|_| {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    ((state >> 40) as f32 / 16_777_216.0) * 2.0 - 1.0
                })
                .collect();
            hnsw.insert(
                NodeId::new(((id_base + index + 1) as u128).to_be_bytes()),
                vector,
            )
            .expect("insert seeded vector");
            bm25.index_document(
                format!("document-{}", id_base + index),
                &format!(
                    "seed {seed} document {index} retrieval topic {} term {}",
                    index % 17,
                    state >> 32
                ),
            )
            .expect("index generated document");
        }
        assert_eq!(hnsw.len(), ROWS);
        assert_eq!(bm25.doc_count(), ROWS);
        let hnsw_value = serde_json::to_value(hnsw.snapshot()).expect("expected HNSW snapshot");
        let bm25_value = serde_json::to_value(&bm25).expect("expected BM25 index");
        Self {
            hnsw,
            bm25,
            hnsw_value,
            bm25_value,
        }
    }
}
