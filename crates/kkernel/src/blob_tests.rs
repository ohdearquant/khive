use std::path::Path;

use super::*;
use khive_db::StorageBackend;

#[test]
fn non_utf8_canonical_path_is_refused_with_member_context() {
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    let non_utf8 = PathBuf::from(OsString::from_vec(b"/roster/archive_\xff.db".to_vec()));
    assert!(non_utf8.to_str().is_none());
    let error = ensure_reportable_path("--with-db archive", &non_utf8)
        .expect_err("the JSON report cannot represent a byte-named roster member");
    let message = error.to_string();
    assert!(message.contains("--with-db archive"), "{message}");
    assert!(message.contains("non-UTF-8"), "{message}");
    assert!(ensure_reportable_path("main", Path::new("/roster/archive.db")).is_ok());
}

fn fixture(path: &Path) -> StorageBackend {
    let backend = StorageBackend::sqlite_for_test(path).expect("create fixture database");
    backend
        .prepare_core_schema()
        .expect("migrate fixture database");
    backend
}

async fn insert_record(backend: &StorageBackend, id: &str, deleted_at: Option<i64>) {
    insert_record_in_namespace(backend, id, "local", deleted_at).await;
}

async fn insert_record_in_namespace(
    backend: &StorageBackend,
    id: &str,
    namespace: &str,
    deleted_at: Option<i64>,
) {
    let sql = backend.sql();
    let mut writer = sql.writer().await.expect("fixture writer");
    writer
            .execute(SqlStatement {
                sql: "INSERT INTO entities (id, namespace, kind, name, created_at, updated_at, deleted_at) \
                      VALUES (?1, ?2, 'concept', 'fixture', 1, 1, ?3)"
                    .into(),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(namespace.to_string()),
                    deleted_at.map_or(SqlValue::Null, SqlValue::Integer),
                ],
                label: None,
            })
            .await
            .expect("insert fixture record");
}

async fn insert_note(backend: &StorageBackend, id: &str) {
    let sql = backend.sql();
    let mut writer = sql.writer().await.expect("fixture writer");
    writer
        .execute(SqlStatement {
            sql: "INSERT INTO notes (id, namespace, kind, created_at, updated_at) \
                      VALUES (?1, 'local', 'observation', 1, 1)"
                .into(),
            params: vec![SqlValue::Text(id.to_string())],
            label: None,
        })
        .await
        .expect("insert fixture note");
}

async fn insert_attachment(backend: &StorageBackend, id: &str) {
    insert_attachment_role(backend, id, "body").await;
}

async fn insert_attachment_role(backend: &StorageBackend, id: &str, role: &str) {
    let sql = backend.sql();
    let mut writer = sql.writer().await.expect("fixture writer");
    writer
            .execute(SqlStatement {
                sql: "INSERT INTO attachments \
                      (record_uuid, substrate, role, content_ref, media_type, size_bytes, created_at) \
                      VALUES (?1, 'entity', ?2, ?3, 'text/plain', 4, 123)"
                    .into(),
                params: vec![
                    SqlValue::Text(id.to_string()),
                    SqlValue::Text(role.to_string()),
                    SqlValue::Text("0".repeat(64)),
                ],
                label: None,
            })
            .await
            .expect("insert fixture attachment");
}

#[tokio::test]
async fn report_paginates_and_probes_every_roster_member_without_deleting() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let main = dir.path().join("main.db");
    let old_backend = dir.path().join("old-backend.db");
    let config = dir.path().join("roster.toml");
    std::fs::write(
        &config,
        format!(
            "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n\
                 [[backends]]\nname = 'unassigned'\nkind = 'sqlite'\npath = {:?}\n",
            main.display().to_string(),
            old_backend.display().to_string()
        ),
    )
    .expect("roster config");
    let main_backend = fixture(&main);
    let secondary = fixture(&old_backend);
    let ids = (1..=130)
        .map(|n| uuid::Uuid::from_u128(n).to_string())
        .collect::<Vec<_>>();
    for id in &ids {
        insert_attachment(&main_backend, id).await;
    }
    // The first role for id[127] ends page one; the second starts page two.
    insert_attachment_role(&main_backend, &ids[127], "metadata").await;
    insert_record_in_namespace(&secondary, &ids[0], "other-namespace", None).await;
    insert_record(&secondary, &ids[1], Some(456)).await;
    let sql = secondary.sql();
    let mut writer = sql.writer().await.expect("fixture writer");
    writer
        .execute(SqlStatement {
            sql: "UPDATE entities SET merged_into = ?1, version = version + 1 WHERE id = ?2".into(),
            params: vec![
                SqlValue::Text(ids[0].clone()),
                SqlValue::Text(ids[1].clone()),
            ],
            label: None,
        })
        .await
        .expect("mark merged tombstone");
    drop(writer);
    drop(sql);
    insert_record(&main_backend, &ids[2], None).await;
    insert_record(&secondary, &ids[2], None).await;
    insert_note(&secondary, &ids[3]).await;
    let sql = main_backend.sql();
    let mut writer = sql.writer().await.expect("fixture writer");
    writer
        .execute(SqlStatement {
            sql: "UPDATE attachments SET substrate = 'note' WHERE record_uuid = ?1".into(),
            params: vec![SqlValue::Text(ids[3].clone())],
            label: None,
        })
        .await
        .expect("mark note attachment");
    drop(writer);
    drop(sql);
    drop(main_backend);
    drop(secondary);
    khive_storage::test_support::freeze_snapshot_sidecars(&main);
    khive_storage::test_support::freeze_snapshot_sidecars(&old_backend);

    let report = ownerless_rows(OwnerlessRowsArgs {
        db: None,
        config: Some(config),
        with_db: vec![old_backend],
    })
    .await
    .expect("read-only report");

    assert_eq!(report.counters.members, 2);
    assert_eq!(report.counters.scanned, 131);
    assert_eq!(report.counters.owned, 4);
    assert_eq!(report.counters.owned_deleted, 1);
    assert_eq!(report.counters.owned_multiple, 1);
    assert_eq!(report.counters.ownerless, 127);
    assert_eq!(report.rows.len(), 127);
    assert!(report.rows.iter().any(|row| row.record_uuid == ids[129]));
    assert!(report
        .rows
        .iter()
        .any(|row| row.record_uuid == ids[127] && row.role == "metadata"));
    assert!(report.rows.iter().all(|row| row.member_probes.len() == 2));
    assert_eq!(report.members[1].names.len(), 2);
    assert!(!report.roster_completeness_verified);
    assert_eq!(
        report.members[0].schema_version,
        khive_db::migrations::latest_schema_version()
    );
    for phrase in [
        "exactly once",
        "behind the cursor",
        "changed or deleted row",
        "Counters are observations",
        "outside the stated roster",
        "older backend copy is restored",
        "never a deletion manifest",
        "committed after the snapshot is not visible",
    ] {
        assert!(report.notice.contains(phrase), "missing notice: {phrase}");
    }

    let backend = StorageBackend::sqlite_read_only(&main).expect("main stays readable");
    let sql = backend.sql();
    let mut reader = sql.reader().await.expect("read attachment count");
    let count = reader
        .query_scalar(SqlStatement {
            sql: "SELECT COUNT(*) FROM attachments".into(),
            params: vec![],
            label: None,
        })
        .await
        .expect("count attachments");
    assert!(matches!(count, Some(SqlValue::Integer(131))));
}

#[tokio::test]
async fn missing_roster_member_refuses_entire_report() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let main = dir.path().join("main.db");
    let config = dir.path().join("empty.toml");
    std::fs::write(&config, "").expect("empty config");
    let backend = fixture(&main);
    let id = uuid::Uuid::from_u128(1).to_string();
    insert_attachment(&backend, &id).await;
    let writer_join = backend
        .pool()
        .take_writer_task_join()
        .expect("fixture writer task started");
    drop(backend);
    tokio::time::timeout(std::time::Duration::from_secs(5), writer_join)
        .await
        .expect("fixture writer task drains")
        .expect("fixture writer task does not panic");
    assert!(!sidecar_path(&main, "-wal").exists());
    assert!(!sidecar_path(&main, "-shm").exists());

    let error = match ownerless_rows(OwnerlessRowsArgs {
        db: Some(main.display().to_string()),
        config: Some(config),
        with_db: vec![dir.path().join("missing.db")],
    })
    .await
    {
        Ok(_) => panic!("missing member must fail closed"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("missing.db"));
}

#[tokio::test]
async fn stale_schema_and_memory_member_refuse_the_report() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let main = dir.path().join("main.db");
    let stale = dir.path().join("stale.db");
    let config = dir.path().join("roster.toml");
    let backend = fixture(&main);
    let id = uuid::Uuid::from_u128(1).to_string();
    insert_attachment(&backend, &id).await;
    drop(backend);
    drop(StorageBackend::sqlite_for_test(&stale).expect("unmigrated sqlite file"));
    khive_storage::test_support::freeze_snapshot_sidecars(&main);
    khive_storage::test_support::freeze_snapshot_sidecars(&stale);
    std::fs::write(&config, "").expect("empty config");
    let error = match ownerless_rows(OwnerlessRowsArgs {
        db: Some(main.display().to_string()),
        config: Some(config.clone()),
        with_db: vec![stale.clone()],
    })
    .await
    {
        Ok(_) => panic!("stale schema must fail closed"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(message.contains("stale.db"));
    assert!(message.contains("behind the latest known migration"));

    std::fs::write(
        &config,
        format!(
            "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n\
                 [[backends]]\nname = 'memory-member'\nkind = 'memory'\n",
            main.display().to_string()
        ),
    )
    .expect("roster config");
    let error = match ownerless_rows(OwnerlessRowsArgs {
        db: None,
        config: Some(config.clone()),
        with_db: vec![],
    })
    .await
    {
        Ok(_) => panic!("memory member must fail closed"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("memory-member"));

    let error = match ownerless_rows(OwnerlessRowsArgs {
        db: Some(":memory:".to_string()),
        config: Some(config),
        with_db: vec![],
    })
    .await
    {
        Ok(_) => panic!("in-memory main override must fail closed"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("--db :memory:"));
}

#[tokio::test]
async fn removed_configured_member_refuses_entire_report() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let main = dir.path().join("main.db");
    let departed = dir.path().join("departed.db");
    let config = dir.path().join("roster.toml");
    let backend = fixture(&main);
    drop(backend);
    khive_storage::test_support::freeze_snapshot_sidecars(&main);
    std::fs::write(
        &config,
        format!(
            "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n\
                 [[backends]]\nname = 'departed'\nkind = 'sqlite'\npath = {:?}\n",
            main.display().to_string(),
            departed.display().to_string()
        ),
    )
    .expect("roster config");
    let error = match ownerless_rows(OwnerlessRowsArgs {
        db: None,
        config: Some(config),
        with_db: vec![],
    })
    .await
    {
        Ok(_) => panic!("removed declaration target must fail closed"),
        Err(error) => error,
    };
    assert!(format!("{error:#}").contains("departed.db"));
}

#[tokio::test]
async fn removing_a_declaration_changes_roster_but_with_db_restores_it() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let main = dir.path().join("main.db");
    let old = dir.path().join("old.db");
    let config = dir.path().join("roster.toml");
    let main_backend = fixture(&main);
    let old_backend = fixture(&old);
    let id = uuid::Uuid::from_u128(1).to_string();
    insert_attachment(&main_backend, &id).await;
    insert_record(&old_backend, &id, None).await;
    drop(main_backend);
    drop(old_backend);
    khive_storage::test_support::freeze_snapshot_sidecars(&main);
    khive_storage::test_support::freeze_snapshot_sidecars(&old);

    let configured = format!(
        "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n\
             [[backends]]\nname = 'old'\nkind = 'sqlite'\npath = {:?}\n",
        main.display().to_string(),
        old.display().to_string()
    );
    std::fs::write(&config, configured).expect("two-member roster");
    let args = |with_db: Vec<PathBuf>| OwnerlessRowsArgs {
        db: None,
        config: Some(config.clone()),
        with_db,
    };
    let first = ownerless_rows(args(vec![])).await.expect("declared owner");
    assert_eq!(first.counters.members, 2);
    assert_eq!(first.counters.owned, 1);
    assert_eq!(first.counters.ownerless, 0);

    std::fs::write(
        &config,
        format!(
            "[[backends]]\nname = 'main'\nkind = 'sqlite'\npath = {:?}\n",
            main.display().to_string()
        ),
    )
    .expect("main-only roster");
    let omitted = ownerless_rows(args(vec![]))
        .await
        .expect("undeclared old backend is not probed");
    assert_eq!(omitted.counters.members, 1);
    assert_eq!(omitted.counters.ownerless, 1);
    assert_eq!(omitted.rows[0].record_uuid, id);

    let restored = ownerless_rows(args(vec![old]))
        .await
        .expect("explicit old backend is probed");
    assert_eq!(restored.counters.members, 2);
    assert_eq!(restored.counters.owned, 1);
    assert_eq!(restored.counters.ownerless, 0);
}

#[tokio::test]
async fn writable_walk_keyset_survives_delete_between_pages() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let main = dir.path().join("main.db");
    let backend = fixture(&main);
    let ids = (1..=130)
        .map(|number| uuid::Uuid::from_u128(number).to_string())
        .collect::<Vec<_>>();
    for id in &ids {
        insert_attachment(&backend, id).await;
    }
    let sql = backend.sql();
    let first = read_page(&sql, None).await.expect("first writable page");
    assert_eq!(first.len(), PAGE_SIZE);
    assert_eq!(first.last().expect("last key").record_uuid, ids[127]);
    let last = first.last().expect("last key");
    let after = (last.record_uuid.clone(), last.role.clone());

    let mut writer = sql.writer().await.expect("fixture writer");
    writer
        .execute(SqlStatement {
            sql: "DELETE FROM attachments WHERE record_uuid = ?1 AND role = 'body'".into(),
            params: vec![SqlValue::Text(ids[0].clone())],
            label: None,
        })
        .await
        .expect("delete a row in the already-read page");
    drop(writer);
    let second = read_page(&sql, Some(&after))
        .await
        .expect("keyset page after deletion");
    let mut reader = sql.reader().await.expect("offset control reader");
    let offset_control = reader
        .query_all(SqlStatement {
            sql: format!(
                "SELECT record_uuid FROM attachments ORDER BY record_uuid COLLATE BINARY, \
                     role COLLATE BINARY LIMIT {PAGE_SIZE} OFFSET {PAGE_SIZE}"
            ),
            params: vec![],
            label: None,
        })
        .await
        .expect("offset control page");
    assert_eq!(offset_control.len(), 1, "offset loses one unread row");
    assert_eq!(
        second.len(),
        2,
        "offset paging would skip one remaining row"
    );
    assert_eq!(second[0].record_uuid, ids[128]);
    assert_eq!(second[1].record_uuid, ids[129]);
}

#[tokio::test]
async fn second_page_probe_error_aborts_without_a_report() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let main = dir.path().join("main.db");
    let config = dir.path().join("empty.toml");
    std::fs::write(&config, "").expect("empty config");
    let backend = fixture(&main);
    for number in 1..=129 {
        insert_attachment(&backend, &uuid::Uuid::from_u128(number).to_string()).await;
    }
    drop(backend);
    khive_storage::test_support::freeze_snapshot_sidecars(&main);
    let args = || OwnerlessRowsArgs {
        db: Some(main.display().to_string()),
        config: Some(config.clone()),
        with_db: vec![],
    };
    let error = match ownerless_rows_with_probe_hook(args(), |page, member| {
        if page == 2 && member == 0 {
            bail!("injected page-two probe failure");
        }
        Ok(())
    })
    .await
    {
        Ok(_) => panic!("late probe failure must abort without a report"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(message.contains("injected page-two probe failure"));
    assert!(message.contains("main"));
}

#[tokio::test]
async fn live_wal_refuses_and_frozen_wal_only_row_is_reported() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let source = dir.path().join("serving.db");
    let frozen = dir.path().join("frozen.db");
    let immutable = dir.path().join("immutable.db");
    let config = dir.path().join("empty.toml");
    std::fs::write(&config, "").expect("empty config");
    let backend = fixture(&source);
    let sql = backend.sql();
    let mut writer = sql.writer().await.expect("fixture writer");
    writer
        .execute_script_top_level(TopLevelMaintenance::WalCheckpointTruncate)
        .await
        .expect("checkpoint migrated schema before writing the candidate row");
    drop(writer);
    drop(sql);
    let id = uuid::Uuid::from_u128(1).to_string();
    insert_attachment(&backend, &id).await;
    let source_wal = sidecar_path(&source, "-wal");
    let source_shm = sidecar_path(&source, "-shm");
    assert!(source_wal.exists() && source_shm.exists());

    let args = |path: &Path| OwnerlessRowsArgs {
        db: Some(path.display().to_string()),
        config: Some(config.clone()),
        with_db: vec![],
    };
    let error = match ownerless_rows(args(&source)).await {
        Ok(_) => panic!("live writable WAL shared memory must be refused"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(message.contains("serving.db"));
    assert!(message.contains("writable WAL shared-memory sidecar"));

    let frozen_wal = sidecar_path(&frozen, "-wal");
    let frozen_shm = sidecar_path(&frozen, "-shm");
    let immutable_wal = sidecar_path(&immutable, "-wal");
    std::fs::copy(&source, &frozen).expect("copy database header before checkpoint");
    std::fs::copy(&source_wal, &frozen_wal).expect("copy committed WAL frames");
    std::fs::copy(&source_shm, &frozen_shm).expect("copy matching WAL index");
    std::fs::copy(&source, &immutable).expect("copy database without WAL frames");
    std::fs::copy(&source_wal, &immutable_wal).expect("copy WAL without shared-memory index");
    let paths = [&frozen, &frozen_wal, &frozen_shm];
    let original_permissions = paths.map(|path| {
        let permissions = std::fs::metadata(path)
            .expect("snapshot metadata")
            .permissions();
        let mut frozen_permissions = permissions.clone();
        frozen_permissions.set_readonly(true);
        std::fs::set_permissions(path, frozen_permissions).expect("freeze snapshot file");
        permissions
    });
    let before = paths.map(|path| std::fs::read(path).expect("read frozen file"));
    assert!(!before[1].is_empty(), "the only copy of the row is in WAL");

    let immutable_uri = format!("file:{}?mode=ro&immutable=1", immutable.display());
    let immutable_connection = rusqlite::Connection::open_with_flags(
        immutable_uri,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
    )
    .expect("test-only immutable control open");
    let immutable_count: i64 = immutable_connection
        .query_row("SELECT COUNT(*) FROM attachments", [], |row| row.get(0))
        .expect("immutable control reads checkpointed schema only");
    assert_eq!(
        immutable_count, 0,
        "immutable SQLite silently misses WAL row"
    );
    drop(immutable_connection);

    let report = ownerless_rows(args(&frozen))
        .await
        .expect("frozen WAL frames must be visible through read-only open");
    assert_eq!(report.counters.scanned, 1);
    assert_eq!(report.rows.len(), 1);
    assert_eq!(report.rows[0].record_uuid, id);
    for (path, (contents, permissions)) in paths
        .into_iter()
        .zip(before.into_iter().zip(original_permissions))
    {
        assert_eq!(std::fs::read(path).expect("read after report"), contents);
        std::fs::set_permissions(path, permissions).expect("restore fixture permissions");
    }
    drop(backend);
}

#[tokio::test]
async fn in_flight_record_refuses_then_abort_and_frozen_copy_lists_candidate() {
    let dir = tempfile::tempdir().expect("fixture dir");
    let source = dir.path().join("main.db");
    let frozen = dir.path().join("frozen.db");
    let config = dir.path().join("empty.toml");
    std::fs::write(&config, "").expect("empty config");
    let backend = fixture(&source);
    let id = uuid::Uuid::from_u128(1).to_string();
    insert_attachment(&backend, &id).await;

    let pending = rusqlite::Connection::open(&source).expect("outside writer");
    pending
        .execute_batch(&format!(
            "BEGIN IMMEDIATE; INSERT INTO entities \
                 (id, namespace, kind, name, created_at, updated_at) \
                 VALUES ('{id}', 'local', 'concept', 'pending', 1, 1);"
        ))
        .expect("hold record publication uncommitted");
    let args = |path: &Path| OwnerlessRowsArgs {
        db: Some(path.display().to_string()),
        config: Some(config.clone()),
        with_db: vec![],
    };
    let error = match ownerless_rows(args(&source)).await {
        Ok(_) => panic!("live writer must be refused while record is uncommitted"),
        Err(error) => error,
    };
    let message = format!("{error:#}");
    assert!(message.contains("main.db"));
    assert!(message.contains("close every live writer"));
    assert!(message.contains("frozen snapshot"));
    pending
        .execute_batch("ROLLBACK")
        .expect("abort publication");
    drop(pending);

    let sql = backend.sql();
    let mut writer = sql.writer().await.expect("fixture writer");
    writer
        .execute_script_top_level(TopLevelMaintenance::WalCheckpointTruncate)
        .await
        .expect("checkpoint after aborted publication");
    drop(writer);
    drop(sql);
    let source_wal = sidecar_path(&source, "-wal");
    let source_shm = sidecar_path(&source, "-shm");
    assert!(source_wal.exists() && source_shm.exists());
    let frozen_wal = sidecar_path(&frozen, "-wal");
    let frozen_shm = sidecar_path(&frozen, "-shm");
    std::fs::copy(&source, &frozen).expect("copy checkpointed main");
    std::fs::copy(&source_wal, &frozen_wal).expect("copy WAL sidecar");
    std::fs::copy(&source_shm, &frozen_shm).expect("copy matching SHM sidecar");
    let paths = [&frozen, &frozen_wal, &frozen_shm];
    let original_permissions = paths.map(|path| {
        let permissions = std::fs::metadata(path)
            .expect("snapshot metadata")
            .permissions();
        let mut frozen_permissions = permissions.clone();
        frozen_permissions.set_readonly(true);
        std::fs::set_permissions(path, frozen_permissions).expect("freeze snapshot file");
        permissions
    });

    let report = ownerless_rows(args(&frozen))
        .await
        .expect("frozen control must list the leftover row");
    assert_eq!(report.counters.scanned, 1);
    assert_eq!(report.counters.ownerless, 1);
    assert_eq!(report.rows[0].record_uuid, id);
    for (path, permissions) in paths.into_iter().zip(original_permissions) {
        std::fs::set_permissions(path, permissions).expect("restore fixture permissions");
    }
    drop(backend);
}
