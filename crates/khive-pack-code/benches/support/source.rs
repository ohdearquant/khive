use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use khive_db::StorageBackend;
use khive_pack_code::source_ingest::{CodeSourceIngestOptions, CodeSourceIngestReport};
use khive_runtime::{KhiveRuntime, RuntimeConfig};
use khive_storage::types::{SqlStatement, SqlValue};
use tempfile::TempDir;

pub const SIZES: [usize; 3] = [10, 1_000, 10_000];

pub fn executor() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .max_blocking_threads(2)
        .enable_all()
        .build()
        .expect("bounded executor")
}

pub struct SourceTree {
    directory: TempDir,
    pub files: usize,
    observed_at: DateTime<Utc>,
}

impl SourceTree {
    pub fn new(files: usize) -> Self {
        assert!(SIZES.contains(&files));
        let physical_temp = std::env::temp_dir()
            .canonicalize()
            .expect("physical temp root");
        let directory = tempfile::tempdir_in(physical_temp).expect("private source tree");
        let src = directory.path().join("src");
        std::fs::create_dir(&src).expect("source directory");
        std::fs::write(
            directory.path().join("Cargo.toml"),
            "[package]\nname = \"bench_generated\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
        )
        .expect("generated manifest");
        // A low-edge corpus isolates scan, mapping, FTS and coverage work.
        // Source code is generated, not compiled; each path has a unique module.
        for index in 0..files {
            let path = if index == 0 {
                "lib.rs".to_owned()
            } else {
                format!("unit_{index:05}.rs")
            };
            std::fs::write(
                src.join(path),
                format!("pub const VALUE_{index}: usize = {index};\n"),
            )
            .expect("generated source file");
        }
        assert_eq!(
            std::fs::read_dir(&src).expect("fixture inventory").count(),
            files
        );
        Self {
            directory,
            files,
            observed_at: "2026-10-17T00:00:00Z".parse().expect("fixed time"),
        }
    }

    pub fn path(&self) -> &Path {
        self.directory.path()
    }

    pub fn options(&self) -> CodeSourceIngestOptions<'_> {
        CodeSourceIngestOptions {
            path: self.path(),
            languages: ["rust"].into_iter().collect(),
            sweep_time: self.observed_at,
            enable_l1: true,
            enable_l1_5: true,
            enable_l2: false,
        }
    }

    pub fn check(&self, report: &CodeSourceIngestReport) {
        assert_eq!(report.projects_created, 1);
        assert_eq!(report.projects_updated, 0);
        assert_eq!(report.modules_created, self.files as u64);
        assert_eq!(report.modules_updated, 0);
        assert!(report.fts_indexed > self.files as u64);
        assert_eq!(report.edges_created, self.files as u64);
        assert_eq!(report.edges_updated, 0);
        assert_eq!(report.unresolved_recorded, 0);
        assert_eq!(report.unresolved_resolved, 0);
        assert_eq!(report.coverage_stamps_missed, 0);
        assert_eq!(report.files_dropped_without_source_path, 0);
        assert_eq!(report.files_skipped_without_module_path, 0);
        assert_eq!(report.source_files_refused, 0);
        assert_eq!(report.manifest_files_refused, 0);
        assert_eq!(report.blocked_count, 0);
        assert!(report.blocked.is_empty());
        assert!(report.l2.is_none());
        assert_eq!(report.languages, ["rust"]);
        assert_eq!(report.source_revision, "unversioned");
        assert_eq!(
            report.warnings,
            [format!(
                "git metadata unavailable for {}; source revision degraded to unversioned",
                self.path().display()
            )]
        );
        let reads = report
            .source_reads
            .as_ref()
            .expect("L1.5 source-read report");
        assert_eq!(reads.files_observed, self.files as u64);
        assert_eq!(reads.literal_reads, 0);
        assert_eq!(reads.resolved_reads, 0);
        assert_eq!(reads.unresolved_reads, 0);
        assert_eq!(reads.coverage_stamps_missed, 0);
    }
}

pub struct MapFixture {
    pub runtime: KhiveRuntime,
    backend: Arc<StorageBackend>,
    directory: TempDir,
    db_path: PathBuf,
}

impl MapFixture {
    pub fn fresh() -> Self {
        let physical_temp = std::env::temp_dir()
            .canonicalize()
            .expect("physical temp root");
        let directory = tempfile::tempdir_in(physical_temp).expect("private map directory");
        let db_path = directory.path().join("map.db");
        let backend = Arc::new(
            StorageBackend::sqlite_for_test_with_journal_mode_in(
                &db_path,
                true,
                Duration::from_secs(10),
                directory.path().join("volume-locks"),
            )
            .expect("queued WAL map"),
        );
        backend.prepare_core_schema().expect("core schema");
        assert!(backend.pool().write_queue_active());
        assert!(backend
            .pool()
            .writer_task_handle()
            .expect("writer initialization")
            .is_some());
        let runtime = KhiveRuntime::from_prepared_backend(
            backend.clone(),
            RuntimeConfig {
                db_path: Some(db_path.clone()),
                packs: vec![],
                actor_id: Some("benchmark:code-ingest".into()),
                ..RuntimeConfig::no_embeddings()
            },
        )
        .expect("prepared no-embedding runtime");
        assert!(runtime.registered_embedding_model_names().is_empty());
        Self {
            runtime,
            backend,
            directory,
            db_path,
        }
    }

    async fn scalar(&self, sql: &str) -> i64 {
        match self
            .runtime
            .sql()
            .reader()
            .await
            .expect("accounting reader")
            .query_scalar(SqlStatement::new(sql, vec![]))
            .await
            .expect("row accounting")
        {
            Some(SqlValue::Integer(value)) => value,
            other => panic!("expected integer: {other:?}"),
        }
    }

    pub async fn counts(&self) -> [i64; 3] {
        [self.scalar("SELECT COUNT(*) FROM entities WHERE deleted_at IS NULL AND namespace='local' AND kind='project'").await,
         self.scalar("SELECT COUNT(*) FROM entities WHERE deleted_at IS NULL AND namespace='local' AND entity_type='module'").await,
         self.scalar("SELECT COUNT(*) FROM graph_edges WHERE deleted_at IS NULL AND namespace='local'").await]
    }

    pub async fn close(self) {
        let Self {
            runtime,
            backend,
            directory,
            db_path,
        } = self;
        drop(runtime);
        let writer = backend
            .pool()
            .take_writer_task_join()
            .expect("queued writer join handle");
        drop(backend);
        tokio::time::timeout(Duration::from_secs(30), writer)
            .await
            .expect("writer teardown deadline")
            .expect("writer joined");
        assert!(std::fs::metadata(db_path).expect("settled map").len() > 0);
        directory.close().expect("remove map and private locks");
    }
}
