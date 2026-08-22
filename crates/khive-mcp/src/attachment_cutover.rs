//! Host-runtime coordination for ADR-121/ADR-160's V21 attachment cutover.

use std::sync::Arc;

use anyhow::Context as _;
use khive_db::migrations::AttachmentCutoverStatus;
use khive_runtime::{BlobHydrator, StorageBackend};
use khive_storage::types::{SqlStatement, SqlValue};
use khive_storage::{Attachment, AttachmentSubstrate};

const FANN_NETWORK_ROLE: &str = "fann-network";

enum CutoverMode {
    Main,
    EmptySecondary { backend_name: String },
}

impl CutoverMode {
    fn secondary_name(&self) -> Option<&str> {
        match self {
            Self::Main => None,
            Self::EmptySecondary { backend_name } => Some(backend_name),
        }
    }
}

#[cfg(test)]
mod test_sync {
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, OnceLock};

    use khive_storage::SqlAccess;
    use tokio::sync::Barrier;

    struct Hook {
        database_path: PathBuf,
        barrier: Arc<Barrier>,
    }

    fn hook() -> &'static Mutex<Option<Hook>> {
        static HOOK: OnceLock<Mutex<Option<Hook>>> = OnceLock::new();
        HOOK.get_or_init(|| Mutex::new(None))
    }

    pub(super) fn install(database_path: PathBuf, barrier: Arc<Barrier>) {
        *hook().lock().expect("cutover test hook lock") = Some(Hook {
            database_path,
            barrier,
        });
    }

    pub(super) fn clear() {
        *hook().lock().expect("cutover test hook lock") = None;
    }

    pub(super) async fn before_gc_owner(sql: &dyn SqlAccess) {
        let barrier = hook()
            .lock()
            .expect("cutover test hook lock")
            .as_ref()
            .filter(|hook| sql.database_path().as_ref() == Some(&hook.database_path))
            .map(|hook| Arc::clone(&hook.barrier));
        if let Some(barrier) = barrier {
            barrier.wait().await;
        }
    }
}

async fn scalar_count(
    backend: &StorageBackend,
    sql: &'static str,
    label: &'static str,
) -> anyhow::Result<u64> {
    let sql_access = backend.sql();
    let mut reader = sql_access
        .reader()
        .await
        .with_context(|| format!("{label}: acquire SQL reader"))?;
    match reader
        .query_scalar(SqlStatement {
            sql: sql.to_string(),
            params: vec![],
            label: Some(label.to_string()),
        })
        .await
        .with_context(|| format!("{label}: execute count"))?
    {
        Some(SqlValue::Integer(count)) if count >= 0 => Ok(count as u64),
        other => anyhow::bail!("{label}: invalid count result {other:?}"),
    }
}

async fn cutover_status(backend: Arc<StorageBackend>) -> anyhow::Result<AttachmentCutoverStatus> {
    tokio::task::spawn_blocking(move || backend.attachment_cutover_status())
        .await
        .context("attachment cutover status task panicked")?
        .context("inspect attachment cutover status")
}

/// Reject attachment evidence on a non-main database before the main database
/// exposes its durable incomplete marker.
///
/// Phase 4 deliberately chooses one liveness authority: attachment-bearing
/// records are main-backend-only. This check includes recoverable soft-deleted
/// legacy entities and already-completed attachment rows.
pub(crate) async fn require_secondary_attachment_empty(
    backend: Arc<StorageBackend>,
    backend_name: &str,
) -> anyhow::Result<()> {
    let status = cutover_status(Arc::clone(&backend)).await?;
    let legacy_refs = match status {
        AttachmentCutoverStatus::Pending | AttachmentCutoverStatus::Incomplete => {
            scalar_count(
                backend.as_ref(),
                "SELECT COUNT(*) FROM entities WHERE content_ref IS NOT NULL",
                "secondary_legacy_attachment_count",
            )
            .await?
        }
        AttachmentCutoverStatus::Complete => 0,
    };
    let attachment_rows = match status {
        AttachmentCutoverStatus::Pending => 0,
        AttachmentCutoverStatus::Incomplete | AttachmentCutoverStatus::Complete => {
            scalar_count(
                backend.as_ref(),
                "SELECT COUNT(*) FROM attachments",
                "secondary_attachment_row_count",
            )
            .await?
        }
    };

    if legacy_refs != 0 || attachment_rows != 0 {
        anyhow::bail!(
            "backend {backend_name:?} contains attachment-bearing records \
             (legacy_refs={legacy_refs}, attachment_rows={attachment_rows}); V21 makes the \
             main backend the sole attachment/GC liveness authority. Move or remove those \
             recoverable records before retrying boot"
        );
    }
    Ok(())
}

/// Finish one pending/incomplete database on the real host Tokio runtime.
///
/// The outer task is ADR-119 tracked and explicitly awaited. It owns the
/// canonical DB GC guard across stage, async verified blob hydration, the one
/// application-attachment transaction, and finalization. Blocking SQLite
/// phases receive ownership of the guard, so caller cancellation cannot release
/// it while native work is still running.
pub(crate) async fn coordinate_attachment_cutover(
    backend: Arc<StorageBackend>,
    hydrator: Option<Arc<BlobHydrator>>,
) -> anyhow::Result<()> {
    coordinate_attachment_cutover_inner(backend, hydrator, CutoverMode::Main).await
}

/// Finish an interrupted empty secondary without ever making it an
/// attachment-liveness authority.
pub(crate) async fn coordinate_empty_secondary_attachment_cutover(
    backend: Arc<StorageBackend>,
    backend_name: &str,
) -> anyhow::Result<()> {
    coordinate_attachment_cutover_inner(
        backend,
        None,
        CutoverMode::EmptySecondary {
            backend_name: backend_name.to_string(),
        },
    )
    .await
}

async fn coordinate_attachment_cutover_inner(
    backend: Arc<StorageBackend>,
    hydrator: Option<Arc<BlobHydrator>>,
    mode: CutoverMode,
) -> anyhow::Result<()> {
    if cutover_status(Arc::clone(&backend)).await? == AttachmentCutoverStatus::Complete {
        if let Some(backend_name) = mode.secondary_name() {
            // Exact-current read-only snapshots must remain query-only: GC
            // ownership creates an advisory lock file, but no mutation or
            // legacy-column race is possible after the V21 drop. Inventory
            // the immutable secondary and return without filesystem writes.
            require_secondary_attachment_empty(backend, backend_name).await?;
        }
        return Ok(());
    }

    let handle = khive_runtime::daemon::spawn_tracked_task(async move {
        let sql = backend.sql();
        #[cfg(test)]
        test_sync::before_gc_owner(sql.as_ref()).await;
        let owner = khive_db::stores::blob::acquire_database_gc_owner(sql.as_ref())
            .await
            .context("acquire canonical database GC owner for V21")?;

        let (backend, owner, already_complete) = tokio::task::spawn_blocking(move || {
            // The optimistic check before task creation is not a lock. A
            // sibling process may complete V21 while this boot waits for the
            // canonical GC owner, so re-check under ownership before issuing
            // any SQL that still names the legacy column.
            if backend.attachment_cutover_status()? == AttachmentCutoverStatus::Complete {
                return Ok::<_, anyhow::Error>((backend, owner, true));
            }
            backend
                .stage_attachment_cutover(&owner)
                .context("commit V21 attachment stage 1")?;
            Ok::<_, anyhow::Error>((backend, owner, false))
        })
        .await
        .context("V21 stage-1 blocking task panicked")??;
        if already_complete {
            if let Some(backend_name) = mode.secondary_name() {
                require_secondary_attachment_empty(backend, backend_name).await?;
            }
            return Ok(());
        }

        let verified = if let Some(backend_name) = mode.secondary_name() {
            // Revalidate after stage while the same GC owner remains held.
            // A legacy writer that raced the earlier inventory is now either
            // visible both as a legacy ref and a staged content attachment,
            // or will be caught by finalization's exact backfill recheck.
            require_secondary_attachment_empty(Arc::clone(&backend), backend_name).await?;
            Vec::new()
        } else {
            let sql = backend.sql();
            let legacy_model_count =
                khive_pack_moodboard::legacy_preference_model_count(sql.as_ref())
                    .await
                    .context("count legacy moodboard preference models")?;
            if legacy_model_count == 0 {
                Vec::new()
            } else {
                let hydrator = hydrator.as_ref().ok_or_else(|| {
                    anyhow::anyhow!(
                        "V21 found {legacy_model_count} legacy moodboard preference model(s), but \
                         no BlobHydrator is configured; configure [storage.blob] and retry boot"
                    )
                })?;
                let verified = khive_pack_moodboard::verify_legacy_preference_attachments(
                    sql.as_ref(),
                    hydrator.as_ref(),
                )
                .await
                .context("verify legacy moodboard bundle/event/FANN evidence")?;
                if verified.len() as u64 != legacy_model_count {
                    anyhow::bail!(
                        "legacy moodboard verification returned {} attachment(s) for \
                         {legacy_model_count} recoverable model(s)",
                        verified.len()
                    );
                }
                verified
            }
        };

        let created_at = chrono::Utc::now().timestamp_micros();
        let attachments = verified
            .into_iter()
            .map(|verified| Attachment {
                record_uuid: verified.model_id,
                substrate: AttachmentSubstrate::Entity,
                role: FANN_NETWORK_ROLE.to_string(),
                content_ref: verified.network_content_ref,
                media_type: Some("application/octet-stream".to_string()),
                size_bytes: Some(verified.size_bytes),
                created_at,
            })
            .collect::<Vec<_>>();

        tokio::task::spawn_blocking(move || {
            backend
                .apply_verified_attachments(&owner, &attachments)
                .context("atomically apply verified pack-owned attachments")?;
            backend
                .finalize_attachment_cutover(&owner)
                .context("finalize V21 attachment/GC cutover")?;
            Ok::<_, anyhow::Error>(())
        })
        .await
        .context("V21 finalization blocking task panicked")??;

        Ok::<_, anyhow::Error>(())
    });

    handle
        .await
        .context("tracked V21 attachment migrator panicked")??;
    Ok(())
}

/// Materialize the exact canonical V20 prefix without invoking V21.
///
/// Test-only upgrade fixture shared by coordinator and multi-backend boot
/// regressions; this is not an alternate production migration path.
#[cfg(test)]
pub(crate) fn create_v20_database_fixture(
    path: &std::path::Path,
    entity_type: &str,
    deleted_at: Option<i64>,
) -> (uuid::Uuid, khive_storage::ContentRef) {
    use khive_db::migrations::{ATTACHMENT_CUTOVER_VERSION, MIGRATIONS};

    let mut conn = rusqlite::Connection::open(path).expect("open V20 fixture");
    conn.execute_batch(
        "CREATE TABLE _schema_migrations (\
             version INTEGER PRIMARY KEY, \
             name TEXT NOT NULL, \
             applied_at INTEGER NOT NULL\
         ) STRICT;",
    )
    .expect("create migration ledger");
    for migration in MIGRATIONS
        .iter()
        .filter(|migration| migration.version < ATTACHMENT_CUTOVER_VERSION)
    {
        let tx = conn.transaction().expect("begin fixture migration");
        tx.execute_batch(migration.up)
            .unwrap_or_else(|error| panic!("apply V{}: {error}", migration.version));
        tx.execute(
            "INSERT INTO _schema_migrations (version, name, applied_at) \
             VALUES (?1, ?2, ?3)",
            rusqlite::params![
                migration.version,
                migration.name,
                i64::from(migration.version)
            ],
        )
        .unwrap_or_else(|error| panic!("record V{}: {error}", migration.version));
        tx.commit().expect("commit fixture migration");
    }

    let id = uuid::Uuid::new_v4();
    let content_ref =
        khive_storage::ContentRef::from_hex("a".repeat(64)).expect("canonical fixture digest");
    conn.execute(
        "INSERT INTO entities (\
             id, namespace, kind, entity_type, name, tags, created_at, updated_at, \
             deleted_at, content_ref\
         ) VALUES (?1, 'local', 'artifact', ?2, 'legacy fixture', '[]', 1, 1, ?3, ?4)",
        rusqlite::params![
            id.to_string(),
            entity_type,
            deleted_at,
            content_ref.as_str()
        ],
    )
    .expect("insert legacy attachment-bearing entity");
    (id, content_ref)
}

#[cfg(test)]
mod tests {
    use khive_db::migrations::ATTACHMENT_CUTOVER_VERSION;
    use khive_db::stores::blob::FsBlobStore;
    use khive_storage::BlobStore as _;

    use super::*;

    #[tokio::test]
    async fn non_model_legacy_content_resumes_to_attachment_only_v21_without_a_blob_store() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("legacy.db");
        let (entity_id, content_ref) = create_v20_database_fixture(&db_path, "visual_asset", None);
        let backend = Arc::new(StorageBackend::sqlite(&db_path).unwrap());

        assert_eq!(
            backend.prepare_core_schema().unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1,
            "ordinary migration must stop before the coordinated legacy cutover"
        );
        coordinate_attachment_cutover(Arc::clone(&backend), None)
            .await
            .expect("non-model legacy content needs no BlobHydrator");
        assert_eq!(
            backend.attachment_cutover_status().unwrap(),
            AttachmentCutoverStatus::Complete
        );

        let attachment = backend
            .attachments()
            .unwrap()
            .get_attachment(entity_id, "content")
            .await
            .unwrap()
            .expect("legacy content role must be backfilled");
        assert_eq!(attachment.content_ref, content_ref);

        let conn = rusqlite::Connection::open(&db_path).unwrap();
        let legacy_columns: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('entities') WHERE name = 'content_ref'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_columns, 0, "final V21 must drop the legacy column");
    }

    #[tokio::test]
    async fn legacy_model_without_hydrator_stays_incomplete_and_gc_refuses_to_run() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("legacy-model.db");
        create_v20_database_fixture(&db_path, "moodboard_model", None);
        let backend = Arc::new(StorageBackend::sqlite(&db_path).unwrap());
        assert_eq!(
            backend.prepare_core_schema().unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1
        );

        let error = coordinate_attachment_cutover(Arc::clone(&backend), None)
            .await
            .expect_err("legacy model evidence requires configured blob hydration");
        assert!(error.to_string().contains("no BlobHydrator is configured"));
        assert_eq!(
            backend.attachment_cutover_status().unwrap(),
            AttachmentCutoverStatus::Incomplete,
            "a failed application stage must remain durably resumable"
        );

        let store = FsBlobStore::new(temp.path().join("blobs"), 0).unwrap();
        let sweep_error = store
            .transactional_orphan_sweep(backend.sql().as_ref(), false)
            .await
            .expect_err("GC must not run over an incomplete dual representation");
        assert!(sweep_error.to_string().contains("fencing"));
    }

    #[tokio::test]
    async fn secondary_preflight_counts_soft_deleted_legacy_references() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("secondary.db");
        create_v20_database_fixture(&db_path, "visual_asset", Some(2));
        let backend = Arc::new(StorageBackend::sqlite(&db_path).unwrap());
        assert_eq!(
            backend.prepare_core_schema().unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1
        );

        let error = require_secondary_attachment_empty(backend, "secondary")
            .await
            .expect_err("recoverable soft-deleted references still participate in liveness");
        assert!(error.to_string().contains("legacy_refs=1"));
    }

    #[tokio::test]
    #[serial_test::serial]
    async fn concurrent_boots_recheck_completion_after_waiting_for_gc_ownership() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("concurrent-boot.db");
        create_v20_database_fixture(&db_path, "visual_asset", None);
        let backend = Arc::new(StorageBackend::sqlite(&db_path).unwrap());
        assert_eq!(
            backend.prepare_core_schema().unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1
        );

        let owner = khive_db::stores::blob::acquire_database_gc_owner(backend.sql().as_ref())
            .await
            .unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        test_sync::install(
            std::fs::canonicalize(&db_path).expect("canonical fixture database"),
            Arc::clone(&barrier),
        );
        let first = tokio::spawn(coordinate_attachment_cutover(Arc::clone(&backend), None));
        let second = tokio::spawn(coordinate_attachment_cutover(Arc::clone(&backend), None));

        tokio::time::timeout(std::time::Duration::from_secs(5), barrier.wait())
            .await
            .expect("both boot tasks must reach the per-database GC-owner hook");
        test_sync::clear();
        drop(owner);

        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(
            backend.attachment_cutover_status().unwrap(),
            AttachmentCutoverStatus::Complete
        );
    }

    #[tokio::test]
    async fn interrupted_secondary_revalidates_empty_after_staging() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("secondary-race.db");
        create_v20_database_fixture(&db_path, "visual_asset", None);
        let backend = Arc::new(StorageBackend::sqlite(&db_path).unwrap());
        assert_eq!(
            backend.prepare_core_schema().unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1
        );
        let owner = khive_db::stores::blob::acquire_database_gc_owner(backend.sql().as_ref())
            .await
            .unwrap();
        backend.stage_attachment_cutover(&owner).unwrap();
        drop(owner);

        let error =
            coordinate_empty_secondary_attachment_cutover(Arc::clone(&backend), "secondary")
                .await
                .expect_err("a raced legacy reference must never become secondary liveness");
        assert!(error.to_string().contains("attachment_rows=1"));
        assert_eq!(
            backend.attachment_cutover_status().unwrap(),
            AttachmentCutoverStatus::Incomplete
        );
    }
}
