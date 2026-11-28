//! Host-runtime coordination for ADR-121/ADR-160's V21 attachment cutover.

use std::sync::Arc;

use anyhow::Context as _;
use khive_db::migrations::AttachmentCutoverStatus;
use khive_storage::types::SqlStatement;
use khive_storage::{Attachment, AttachmentSubstrate, SqlAccess};

use crate::{BlobHydrator, LegacyPreferenceVerifier, RuntimeError, StorageBackend};

const FANN_NETWORK_ROLE: &str = "fann-network";

/// Count legacy (pre-V21) moodboard preference-model artifact rows,
/// including soft-deleted ones.
///
/// This is the core-side twin of `khive_pack_moodboard::legacy_preference_model_count`
/// (same SQL, same label) — it lets V21 detect legacy moodboard rows without
/// linking `khive-pack-moodboard`, which is an optional dependency. The
/// query itself uses no moodboard types: it is a plain count over `entities`.
/// `'moodboard_model'` is a value in the data, not a code dependency on the
/// pack.
pub async fn legacy_preference_model_count(sql: &dyn SqlAccess) -> Result<u64, RuntimeError> {
    let mut reader = sql.reader().await?;
    Ok(reader
        .count(SqlStatement {
            sql: "SELECT COUNT(*) FROM entities \
                  WHERE kind = 'artifact' AND entity_type = 'moodboard_model' \
                    AND content_ref IS NOT NULL"
                .to_string(),
            params: vec![],
            label: Some("moodboard_legacy_preference_model_count".to_string()),
        })
        .await?)
}

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
    reader
        .count(SqlStatement {
            sql: sql.to_string(),
            params: vec![],
            label: Some(label.to_string()),
        })
        .await
        .with_context(|| format!("{label}: execute count"))
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
pub async fn require_secondary_attachment_empty(
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
///
/// This is the public entry point an embedding host calls to bring a V20
/// database to V21. `KhiveRuntime::new` migrates a writable database to V20
/// and, when V21 is left to this coordinator, refuses it with
/// [`RuntimeError::AttachmentCutoverRequired`]; run this function over the
/// same database, then construct the runtime again.
///
/// The cutover stages only from V20. A file below V20 opened directly with a
/// backend constructor is refused here until
/// [`StorageBackend::prepare_core_schema`] has migrated it forward (to V20, or
/// through V21 when no legacy reference needs this coordinator).
///
/// `Ok(())` means the V21 cutover is complete. Whether the later migrations
/// ran depends on the state at entry. When the cutover was already complete,
/// this returns at once and runs nothing, so the schema can still be behind the
/// latest version. Otherwise, once the cutover is finished here (or by a
/// sibling while this call waited for the GC owner), it applies the remaining
/// migrations before returning. `KhiveRuntime::new` applies them in either
/// case; `KhiveRuntime::from_prepared_backend` needs
/// [`StorageBackend::prepare_core_schema`] called first.
///
/// A database that holds legacy moodboard preference models needs both a
/// `verifier` and a `hydrator`; with either one `None`, the call fails closed
/// and the cutover stays resumable.
///
/// # Inspecting the state without migrating
///
/// [`StorageBackend::attachment_cutover_status`] is public and only reads: it
/// takes a pooled reader and inspects the schema bookkeeping, so it can be
/// called on a pre-V21 file that has been opened with a backend constructor but
/// not yet prepared. It reports `Pending`, `Incomplete` or `Complete`; it never
/// runs `prepare_core_schema` and never advances the database.
///
/// # Arguments
///
/// * `hydrator` - verified blob hydration; required when legacy attachment
///   evidence exists, otherwise `None` is accepted.
/// * `verifier` - the host's legacy-preference verifier, or `None` when the
///   host has none.
///
/// # Errors
///
/// Returns an error when the status cannot be read, when the database is below
/// V20 and has not been prepared, when legacy evidence
/// requires a hydrator that was not configured, or when staging, application or
/// finalization fails. A failed run leaves the cutover durably resumable.
pub async fn coordinate_attachment_cutover(
    backend: Arc<StorageBackend>,
    hydrator: Option<Arc<BlobHydrator>>,
    verifier: Option<Arc<dyn LegacyPreferenceVerifier>>,
) -> anyhow::Result<()> {
    coordinate_attachment_cutover_inner(backend, hydrator, verifier, CutoverMode::Main).await
}

/// Finish an interrupted empty secondary without ever making it an
/// attachment-liveness authority.
pub async fn coordinate_empty_secondary_attachment_cutover(
    backend: Arc<StorageBackend>,
    backend_name: &str,
) -> anyhow::Result<()> {
    coordinate_attachment_cutover_inner(
        backend,
        None,
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
    verifier: Option<Arc<dyn LegacyPreferenceVerifier>>,
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

    let prepared = Arc::clone(&backend);
    // The tracked child runs on its own task: every poll of it, and every write it
    // queues, is migration class. Its blocking hops call cutover methods that scope
    // themselves.
    let migration = khive_db::WriteClassContext::migration();
    let handle = crate::spawn_named_tracked_task(
        "attachment_cutover",
        migration.scope_future(async move {
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
                let legacy_model_count = legacy_preference_model_count(sql.as_ref())
                    .await
                    .context("count legacy moodboard preference models")?;
                if legacy_model_count == 0 {
                    Vec::new()
                } else {
                    let verifier = verifier.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                        "V21 found {legacy_model_count} legacy moodboard preference model(s), but \
                         this build has no moodboard support (compiled without the \
                         `pack-moodboard` feature); rebuild with that feature enabled to \
                         migrate them, then retry boot"
                    )
                    })?;
                    let hydrator = hydrator.as_ref().ok_or_else(|| {
                        anyhow::anyhow!(
                        "V21 found {legacy_model_count} legacy moodboard preference model(s), but \
                         no BlobHydrator is configured; configure [storage.blob] and retry boot"
                    )
                    })?;
                    let verified = verifier
                        .verify_legacy_preference_attachments(sql.as_ref(), hydrator.as_ref())
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
        }),
    );

    handle
        .await
        .context("tracked V21 attachment migrator panicked")??;

    // Ordinary schema preparation stops before V21, so a backend whose cutover
    // was just finished here (or by a sibling while this boot waited for the GC
    // owner) is still at V21. Apply the remaining migrations before the runtime
    // is built on it; the GC owner was released when the task above ended.
    tokio::task::spawn_blocking(migration.wrap_blocking(move || prepared.prepare_core_schema()))
        .await
        .context("post-V21 schema preparation task panicked")?
        .context("apply the migrations after the V21 cutover")?;
    Ok(())
}

#[cfg(test)]
#[path = "../tests/support/v20_fixture.rs"]
mod v20_fixture;

#[cfg(test)]
mod tests {
    use khive_db::migrations::ATTACHMENT_CUTOVER_VERSION;
    use khive_db::stores::blob::FsBlobStore;
    use khive_storage::BlobStore as _;

    use super::v20_fixture::create_v20_database_fixture;
    use super::*;

    #[tokio::test]
    async fn legacy_model_count_includes_recoverable_models_only() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("legacy-model-count.db");
        let (_, content_ref) = create_v20_database_fixture(&db_path, "moodboard_model", None);
        let conn = rusqlite::Connection::open(&db_path).unwrap();
        for (kind, entity_type, deleted_at, content_ref) in [
            (
                "artifact",
                "moodboard_model",
                Some(2),
                Some(content_ref.as_str()),
            ),
            ("artifact", "visual_asset", None, Some(content_ref.as_str())),
            ("artifact", "moodboard_model", None, None),
            (
                "document",
                "moodboard_model",
                None,
                Some(content_ref.as_str()),
            ),
        ] {
            conn.execute(
                "INSERT INTO entities (id, namespace, kind, entity_type, name, tags, \
                 created_at, updated_at, deleted_at, content_ref) \
                 VALUES (?1, 'other', ?2, ?3, 'count fixture', '[]', 1, 1, ?4, ?5)",
                rusqlite::params![
                    uuid::Uuid::new_v4().to_string(),
                    kind,
                    entity_type,
                    deleted_at,
                    content_ref
                ],
            )
            .unwrap();
        }
        drop(conn);
        let backend = StorageBackend::sqlite_for_test(&db_path).unwrap();
        backend.prepare_core_schema().unwrap();
        assert_eq!(
            legacy_preference_model_count(backend.sql().as_ref())
                .await
                .unwrap(),
            2
        );
    }

    #[tokio::test]
    async fn secondary_preflight_counts_across_cutover_states_and_propagates_errors() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("secondary-count.db");
        let (entity_id, content_ref) = create_v20_database_fixture(&db_path, "visual_asset", None);
        let backend = Arc::new(StorageBackend::sqlite_for_test(&db_path).unwrap());
        assert_eq!(
            backend.prepare_core_schema().unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1
        );
        assert_eq!(
            backend.attachment_cutover_status().unwrap(),
            AttachmentCutoverStatus::Pending
        );
        assert_eq!(
            rusqlite::Connection::open(&db_path)
                .unwrap()
                .execute("UPDATE entities SET content_ref = NULL", [])
                .unwrap(),
            1
        );
        require_secondary_attachment_empty(Arc::clone(&backend), "secondary")
            .await
            .expect("pending schema has no attachments table");

        let owner = khive_db::stores::blob::acquire_database_gc_owner(backend.sql().as_ref())
            .await
            .unwrap();
        backend.stage_attachment_cutover(&owner).unwrap();
        assert_eq!(
            backend.attachment_cutover_status().unwrap(),
            AttachmentCutoverStatus::Incomplete
        );
        require_secondary_attachment_empty(Arc::clone(&backend), "secondary")
            .await
            .unwrap();
        backend.finalize_attachment_cutover(&owner).unwrap();
        drop(owner);
        assert_eq!(
            backend.attachment_cutover_status().unwrap(),
            AttachmentCutoverStatus::Complete
        );
        require_secondary_attachment_empty(Arc::clone(&backend), "secondary")
            .await
            .expect("complete schema has no legacy content_ref column");

        rusqlite::Connection::open(&db_path)
            .unwrap()
            .execute(
                "INSERT INTO attachments (record_uuid, substrate, role, content_ref, created_at) \
                 VALUES (?1, 'entity', 'content', ?2, 1)",
                rusqlite::params![entity_id.to_string(), content_ref.as_str()],
            )
            .unwrap();
        let error = require_secondary_attachment_empty(Arc::clone(&backend), "secondary")
            .await
            .expect_err("completed attachment rows still participate in liveness");
        assert!(error
            .to_string()
            .contains("legacy_refs=0, attachment_rows=1"));

        let error = scalar_count(
            &backend,
            "SELECT COUNT(*) FROM absent_attachment_count_fixture",
            "missing_attachment_count",
        )
        .await
        .expect_err("a failed count must never become zero");
        assert_eq!(error.to_string(), "missing_attachment_count: execute count");
        assert!(matches!(
            error.downcast_ref::<khive_storage::StorageError>(),
            Some(khive_storage::StorageError::Driver { .. })
        ));
    }

    #[tokio::test]
    async fn non_model_legacy_content_resumes_to_attachment_only_v21_without_a_blob_store() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("legacy.db");
        let (entity_id, content_ref) = create_v20_database_fixture(&db_path, "visual_asset", None);
        let backend = Arc::new(StorageBackend::sqlite_for_test(&db_path).unwrap());

        assert_eq!(
            backend.prepare_core_schema().unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1,
            "ordinary migration must stop before the coordinated legacy cutover"
        );
        coordinate_attachment_cutover(Arc::clone(&backend), None, None)
            .await
            .expect("non-model legacy content needs no BlobHydrator");
        assert_eq!(
            backend.attachment_cutover_status().unwrap(),
            AttachmentCutoverStatus::Complete
        );
        assert_eq!(
            backend.schema_version().unwrap(),
            khive_db::migrations::latest_schema_version(),
            "the coordinated cutover must leave the schema at the latest migration, \
             not at V21, before a runtime is built on it"
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

    /// Legacy count > 0 and no verifier installed (the default, feature-off
    /// build) must fail closed rather than silently dropping the legacy
    /// column with unmigrated models still on disk.
    ///
    /// Was: `legacy_model_without_hydrator_stays_incomplete_and_gc_refuses_to_run`,
    /// which asserted the error contained "no BlobHydrator is configured".
    /// Now: no verifier is ever passed in this test, so the *new* fail-closed
    /// arm fires first — before hydrator configuration is even consulted —
    /// and the assertion moves to the new message naming the legacy count
    /// and the missing `pack-moodboard` feature.
    #[tokio::test]
    async fn legacy_model_without_verifier_fails_closed_and_gc_refuses_to_run() {
        let temp = tempfile::tempdir().unwrap();
        let db_path = temp.path().join("legacy-model.db");
        create_v20_database_fixture(&db_path, "moodboard_model", None);
        let backend = Arc::new(StorageBackend::sqlite_for_test(&db_path).unwrap());
        assert_eq!(
            backend.prepare_core_schema().unwrap(),
            ATTACHMENT_CUTOVER_VERSION - 1
        );

        let error = coordinate_attachment_cutover(Arc::clone(&backend), None, None)
            .await
            .expect_err("legacy model evidence requires an installed verifier");
        let message = error.to_string();
        assert!(
            message.contains("1 legacy moodboard preference model"),
            "error must name the legacy count: {message}"
        );
        assert!(
            message.contains("pack-moodboard"),
            "error must tell the operator which feature to rebuild with: {message}"
        );
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
        let backend = Arc::new(StorageBackend::sqlite_for_test(&db_path).unwrap());
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
        let backend = Arc::new(StorageBackend::sqlite_for_test(&db_path).unwrap());
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
        let first = tokio::spawn(coordinate_attachment_cutover(
            Arc::clone(&backend),
            None,
            None,
        ));
        let second = tokio::spawn(coordinate_attachment_cutover(
            Arc::clone(&backend),
            None,
            None,
        ));

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
        let backend = Arc::new(StorageBackend::sqlite_for_test(&db_path).unwrap());
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
