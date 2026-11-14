use khive_db::migrations::MIGRATIONS;
use khive_runtime::{KhiveRuntime, RuntimeConfig, StorageBackend};
use khive_storage::types::{SqlStatement, SqlValue};
use serde_json::{json, Value};

use super::{bounded, initialize, put, read, rows, MirrorTree};
use crate::cursor::CursorRow;

const ENTITY_ID: &str = "60810000-0000-4000-8000-000000000001";

async fn ledger(backend: &StorageBackend) -> Vec<(i64, String, i64)> {
    let mut reader = backend.sql().reader().await.unwrap();
    reader
        .query_all(SqlStatement::new(
            "SELECT version, name, applied_at FROM _schema_migrations ORDER BY version",
            vec![],
        ))
        .await
        .unwrap()
        .into_iter()
        .map(|row| {
            (
                row.i64("version").unwrap(),
                row.text("name").unwrap().to_owned(),
                row.i64("applied_at").unwrap(),
            )
        })
        .collect()
}

async fn has_object(backend: &StorageBackend, kind: &str, name: &str) -> bool {
    let mut reader = backend.sql().reader().await.unwrap();
    reader
        .query_row(SqlStatement::new(
            "SELECT name FROM sqlite_master WHERE type = ?1 AND name = ?2",
            vec![SqlValue::Text(kind.into()), SqlValue::Text(name.into())],
        ))
        .await
        .unwrap()
        .is_some()
}

async fn entity_snapshot(backend: &StorageBackend) -> Value {
    let mut reader = backend.sql().reader().await.unwrap();
    let entities = reader
        .query_all(SqlStatement::new(
            "SELECT id, namespace, kind, name, description, properties, tags, \
             created_at, updated_at, deleted_at, entity_type, merged_into, merge_event_id, \
             version FROM entities ORDER BY id",
            vec![],
        ))
        .await
        .unwrap();
    assert_eq!(
        entities.len(),
        1,
        "the predecessor's graph row must survive"
    );
    let row = &entities[0];
    json!({
        "id": row.text("id").unwrap(),
        "namespace": row.text("namespace").unwrap(),
        "kind": row.text("kind").unwrap(),
        "name": row.text("name").unwrap(),
        "description": row.opt_text("description").unwrap(),
        "properties": row.opt_text("properties").unwrap(),
        "tags": row.text("tags").unwrap(),
        "created_at": row.i64("created_at").unwrap(),
        "updated_at": row.i64("updated_at").unwrap(),
        "deleted_at": row.opt_i64("deleted_at").unwrap(),
        "entity_type": row.opt_text("entity_type").unwrap(),
        "merged_into": row.opt_text("merged_into").unwrap(),
        "merge_event_id": row.opt_text("merge_event_id").unwrap(),
        "version": row.i64("version").unwrap(),
    })
}

async fn assert_cursor_shape(runtime: &KhiveRuntime) {
    let mut reader = runtime.sql().reader().await.unwrap();
    let columns = reader
        .query_all(SqlStatement::new(
            "PRAGMA table_info(workspace_mirror_cursor)",
            vec![],
        ))
        .await
        .unwrap();
    let shape: Vec<_> = columns
        .iter()
        .map(|column| {
            (
                column.text("name").unwrap(),
                column.text("type").unwrap(),
                column.i64("notnull").unwrap(),
                column.i64("pk").unwrap(),
            )
        })
        .collect();
    assert_eq!(
        shape,
        vec![
            ("path", "TEXT", 0, 1),
            ("last_mtime", "INTEGER", 0, 0),
            ("last_hash", "TEXT", 0, 0),
            ("last_synced_at", "INTEGER", 0, 0),
        ]
    );
}

#[tokio::test]
async fn previous_schema_store_upgrade_preserves_rows_and_installs_cursor() {
    bounded(async {
        let (latest, predecessors) = MIGRATIONS.split_last().expect("core migrations exist");
        let previous = predecessors.last().expect("a predecessor migration exists");
        let stop_marker = format!("workspace-cursor-stop-v{}", latest.version);
        let tree = MirrorTree::new();
        let db_path = tree.0.join("historical.db");
        let lock_dir = tree.0.join("locks");
        let expected_entity = json!({
            "id": ENTITY_ID,
            "namespace": "local",
            "kind": "concept",
            "name": "pre-cursor concept",
            "description": "preserved description",
            "properties": "{\"fixture\":true}",
            "tags": "[\"historical\"]",
            "created_at": 101,
            "updated_at": 102,
            "deleted_at": null,
            "entity_type": null,
            "merged_into": null,
            "merge_event_id": null,
            "version": 1,
        });
        let predecessor_ledger = {
            let backend =
                StorageBackend::sqlite_with_volume_lock_dir(&db_path, lock_dir.clone()).unwrap();
            let mut writer = backend.sql().writer().await.unwrap();
            writer
                .execute_script(
                    include_str!(concat!(
                        env!("CARGO_MANIFEST_DIR"),
                        "/../khive-db/sql/schema-migrations-table.sql"
                    ))
                    .into(),
                )
                .await
                .unwrap();
            writer
                .execute_script(format!(
                    "CREATE TRIGGER cursor_fixture_stop_latest \
                     BEFORE INSERT ON _schema_migrations WHEN NEW.version = {} \
                     BEGIN SELECT RAISE(ABORT, '{stop_marker}'); END;",
                    latest.version
                ))
                .await
                .unwrap();
            drop(writer);

            // Build every migration except the newest with the current runner,
            // including Rust steps. This is not an older binary's database.
            let error = backend
                .prepare_core_schema()
                .expect_err("stop before the newest migration commits");
            assert_eq!(
                error.to_string(),
                format!("migration v{} failed: {stop_marker}", latest.version)
            );
            assert_eq!(backend.schema_version().unwrap(), previous.version);
            let prior = ledger(&backend).await;
            assert_eq!(
                prior
                    .iter()
                    .map(|entry| (entry.0, entry.1.as_str()))
                    .collect::<Vec<_>>(),
                predecessors
                    .iter()
                    .map(|migration| (i64::from(migration.version), migration.name))
                    .collect::<Vec<_>>()
            );
            assert!(prior.iter().all(|entry| entry.2 > 0));
            assert!(!has_object(&backend, "table", "workspace_mirror_cursor").await);

            let mut writer = backend.sql().writer().await.unwrap();
            writer
                .execute_script("DROP TRIGGER cursor_fixture_stop_latest;".into())
                .await
                .unwrap();
            writer
                .execute(SqlStatement::new(
                    "INSERT INTO entities \
                     (id, namespace, kind, name, description, properties, tags, \
                      created_at, updated_at, deleted_at, entity_type, merged_into, \
                      merge_event_id, version) \
                     VALUES (?1, 'local', 'concept', 'pre-cursor concept', \
                      'preserved description', '{\"fixture\":true}', '[\"historical\"]', \
                      101, 102, NULL, NULL, NULL, NULL, 1)",
                    vec![SqlValue::Text(ENTITY_ID.into())],
                ))
                .await
                .unwrap();
            drop(writer);
            assert_eq!(entity_snapshot(&backend).await, expected_entity);
            prior
        };

        let expected_cursors = vec![
            CursorRow {
                path: Some("a/../b".into()),
                last_mtime: Some(-7),
                last_hash: Some("opaque prior hash".into()),
                last_synced_at: Some(0),
            },
            CursorRow {
                path: Some("b".into()),
                last_mtime: None,
                last_hash: Some("".into()),
                last_synced_at: None,
            },
        ];
        let upgraded_ledger = {
            let runtime = KhiveRuntime::new(RuntimeConfig {
                db_path: Some(db_path.clone()),
                volume_lock_dir: Some(lock_dir.clone()),
                ..RuntimeConfig::no_embeddings()
            })
            .expect("normal writable runtime upgrades the predecessor store");
            assert_eq!(runtime.backend().schema_version().unwrap(), latest.version);
            let upgraded = ledger(runtime.backend()).await;
            assert_eq!(upgraded.len(), MIGRATIONS.len());
            assert_eq!(
                &upgraded[..predecessors.len()],
                predecessor_ledger.as_slice()
            );
            let last = upgraded.last().unwrap();
            assert_eq!(last.0, i64::from(latest.version));
            assert_eq!(last.1, latest.name);
            assert!(last.2 > 0);
            assert_eq!(entity_snapshot(runtime.backend()).await, expected_entity);
            assert!(!has_object(runtime.backend(), "table", "workspace_mirror_cursor").await);

            initialize(&runtime);
            assert_cursor_shape(&runtime).await;
            assert!(rows(&runtime).await.is_empty());
            for cursor in &expected_cursors {
                put(
                    &runtime,
                    cursor.path.as_deref().unwrap(),
                    cursor.last_mtime,
                    cursor.last_hash.as_deref(),
                    cursor.last_synced_at,
                )
                .await;
                assert_eq!(
                    read(&runtime, cursor.path.as_deref().unwrap()).await,
                    Some(cursor.clone())
                );
            }
            assert_eq!(rows(&runtime).await, expected_cursors);
            initialize(&runtime);
            assert_cursor_shape(&runtime).await;
            assert_eq!(rows(&runtime).await, expected_cursors);
            assert_eq!(entity_snapshot(runtime.backend()).await, expected_entity);
            upgraded
        };

        {
            let runtime = KhiveRuntime::new(RuntimeConfig {
                db_path: Some(db_path),
                volume_lock_dir: Some(lock_dir),
                ..RuntimeConfig::no_embeddings()
            })
            .expect("reopen the upgraded store after all prior handles drop");
            assert_eq!(ledger(runtime.backend()).await, upgraded_ledger);
            assert_cursor_shape(&runtime).await;
            assert_eq!(rows(&runtime).await, expected_cursors);
            assert_eq!(entity_snapshot(runtime.backend()).await, expected_entity);
        }
    })
    .await;
}
