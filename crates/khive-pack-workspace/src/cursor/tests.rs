use std::any::Any;
use std::collections::HashMap;
use std::future::Future;
use std::time::Duration;

use khive_pack_git::GitPack;
use khive_pack_gtd::GtdPack;
use khive_pack_kg::KgPack;
use khive_pack_session::SessionPack;
use khive_runtime::{KhiveRuntime, VerbRegistry, VerbRegistryBuilder};

use super::*;
use crate::pack::WorkspacePack;

async fn bounded(test: impl Future<Output = ()>) {
    tokio::time::timeout(Duration::from_secs(10), test)
        .await
        .expect("workspace cursor test exceeded its async deadline");
}

fn registry(runtime: &KhiveRuntime) -> VerbRegistry {
    let mut builder = VerbRegistryBuilder::new();
    builder.register(KgPack::new(runtime.clone()));
    builder.register(GtdPack::new(runtime.clone()));
    builder.register(GitPack::new(runtime.clone()));
    builder.register(SessionPack::new(runtime.clone()));
    builder.register(WorkspacePack::new(runtime.clone()));
    builder.build().expect("workspace registry")
}

fn initialize(runtime: &KhiveRuntime) {
    registry(runtime)
        .apply_schema_plans_with_map(&HashMap::new(), runtime.backend())
        .expect("fallible writable pack schema initialization");
}

fn fixture() -> KhiveRuntime {
    let runtime = KhiveRuntime::memory().expect("isolated memory runtime");
    initialize(&runtime);
    runtime
}

async fn read(runtime: &KhiveRuntime, path: &str) -> Option<CursorRow> {
    let mut reader = runtime.sql().reader().await.expect("cursor reader");
    get(&mut *reader, path).await.expect("cursor get")
}

async fn rows(runtime: &KhiveRuntime) -> Vec<CursorRow> {
    let mut reader = runtime.sql().reader().await.expect("cursor list reader");
    list(&mut *reader).await.expect("cursor list")
}

async fn put(
    runtime: &KhiveRuntime,
    path: &str,
    mtime: Option<i64>,
    hash: Option<&str>,
    synced: Option<i64>,
) {
    let path = path.to_owned();
    let hash = hash.map(str::to_owned);
    runtime
        .sql()
        .atomic_unit(Box::new(move |writer| {
            Box::pin(async move {
                upsert_on_writer(writer, &path, mtime, hash.as_deref(), synced).await?;
                Ok(Box::new(()) as Box<dyn Any + Send>)
            })
        }))
        .await
        .expect("cursor commit");
}

#[tokio::test]
async fn fresh_registry_initialization_installs_exact_four_columns() {
    bounded(async {
        let runtime = KhiveRuntime::memory().expect("isolated memory runtime");
        let mut reader = runtime.sql().reader().await.unwrap();
        let absent = reader
            .query_row(SqlStatement::new(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = ?1",
                vec![SqlValue::Text("workspace_mirror_cursor".into())],
            ))
            .await
            .unwrap();
        assert!(
            absent.is_none(),
            "fixture must start before workspace schema initialization"
        );
        drop(reader);
        initialize(&runtime);
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
            ],
            "cursor schema must retain exactly the accepted four-column shape"
        );
        assert!(list(&mut *reader).await.unwrap().is_empty());
    })
    .await;
}

#[tokio::test]
async fn existing_store_and_cursor_rows_survive_initialization_and_reapplication() {
    bounded(async {
        let runtime = KhiveRuntime::memory().expect("isolated memory runtime");
        // Model an already initialized database that predates the workspace table.
        // This is not a claim of opening a historical core-version fixture.
        let mut writer = runtime.sql().writer().await.unwrap();
        writer
            .execute_script("CREATE TABLE cursor_prior_fixture (value TEXT);".into())
            .await
            .unwrap();
        writer
            .execute(SqlStatement::new(
                "INSERT INTO cursor_prior_fixture(value) VALUES (?1)",
                vec![SqlValue::Text("existing row".into())],
            ))
            .await
            .unwrap();
        drop(writer);
        initialize(&runtime);
        put(
            &runtime,
            "prior/path",
            Some(-7),
            Some("opaque hash"),
            Some(0),
        )
        .await;
        let before = rows(&runtime).await;
        initialize(&runtime);
        assert_eq!(
            rows(&runtime).await,
            before,
            "idempotent initialization erased cursor data"
        );
        let mut reader = runtime.sql().reader().await.unwrap();
        let prior = reader
            .query_all(SqlStatement::new(
                "SELECT value FROM cursor_prior_fixture",
                vec![],
            ))
            .await
            .unwrap();
        assert_eq!(prior.len(), 1);
        assert_eq!(prior[0].text("value").unwrap(), "existing row");
    })
    .await;
}

#[tokio::test]
async fn exact_path_replacement_preserves_other_paths_and_opaque_nullable_values() {
    bounded(async {
        let runtime = fixture();
        assert_eq!(read(&runtime, "a/../b").await, None);
        put(
            &runtime,
            "a/../b",
            Some(i64::MIN),
            Some("not-a-generated-hash"),
            Some(-1),
        )
        .await;
        put(&runtime, "b", Some(0), Some(""), Some(i64::MAX)).await;
        let other = read(&runtime, "b").await.unwrap();
        assert_eq!(other.last_mtime, Some(0));
        assert_eq!(other.last_hash.as_deref(), Some(""));
        assert_eq!(other.last_synced_at, Some(i64::MAX));
        let first = read(&runtime, "a/../b").await.unwrap();
        assert_eq!(first.last_mtime, Some(i64::MIN));
        assert_eq!(first.last_hash.as_deref(), Some("not-a-generated-hash"));
        assert_eq!(first.last_synced_at, Some(-1));
        put(&runtime, "a/../b", None, None, None).await;
        assert_eq!(
            read(&runtime, "a/../b").await,
            Some(CursorRow {
                path: Some("a/../b".into()),
                last_mtime: None,
                last_hash: None,
                last_synced_at: None,
            }),
            "replacement must preserve explicit NULL, not coalesce old values"
        );
        assert_eq!(read(&runtime, "b").await, Some(other.clone()));
        let all = rows(&runtime).await;
        assert_eq!(
            all.len(),
            2,
            "replace must not insert another row or normalize paths"
        );
        assert_eq!(all[0].path.as_deref(), Some("a/../b"));
        assert_eq!(all[1], other);
    })
    .await;
}

#[tokio::test]
async fn list_retains_schema_permitted_null_path_rows() {
    bounded(async {
        let runtime = fixture();
        runtime
            .sql()
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    writer
                        .execute(SqlStatement::new(
                            "INSERT INTO workspace_mirror_cursor \
                             (path, last_mtime, last_hash, last_synced_at) VALUES (?1, ?2, ?3, ?4)",
                            vec![
                                SqlValue::Null,
                                SqlValue::Integer(-8),
                                SqlValue::Text("".into()),
                                SqlValue::Null,
                            ],
                        ))
                        .await?;
                    Ok(Box::new(()) as Box<dyn Any + Send>)
                })
            }))
            .await
            .unwrap();
        assert_eq!(
            rows(&runtime).await,
            vec![CursorRow {
                path: None,
                last_mtime: Some(-8),
                last_hash: Some("".into()),
                last_synced_at: None,
            }]
        );
        assert_eq!(
            read(&runtime, "").await,
            None,
            "NULL path must not become empty text"
        );
    })
    .await;
}

#[tokio::test]
async fn atomic_error_restores_existing_cursor_and_removes_new_cursor() {
    bounded(async {
        let runtime = fixture();
        put(&runtime, "existing", Some(1), Some("before"), Some(2)).await;
        put(&runtime, "unrelated", Some(3), Some("other"), Some(4)).await;
        let before = rows(&runtime).await;
        let error = runtime
            .sql()
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    upsert_on_writer(writer, "existing", Some(9), Some("after"), Some(10)).await?;
                    upsert_on_writer(writer, "new", Some(11), Some("new"), Some(12)).await?;
                    Err(StorageError::InvalidInput {
                        capability: StorageCapability::Sql,
                        operation: "cursor rollback fixture".into(),
                        message: "abort after both cursor statements".into(),
                    })
                })
            }))
            .await
            .expect_err("deliberate transaction failure must propagate");
        assert!(error
            .to_string()
            .contains("abort after both cursor statements"));
        assert_eq!(
            rows(&runtime).await,
            before,
            "failed atomic unit advanced progress"
        );
        assert_eq!(read(&runtime, "new").await, None);
    })
    .await;
}

#[tokio::test]
async fn actual_sql_write_failure_rolls_back_cursor_changes() {
    bounded(async {
        let runtime = fixture();
        put(&runtime, "existing", Some(1), Some("before"), Some(2)).await;
        let before = rows(&runtime).await;
        let result = runtime
            .sql()
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    upsert_on_writer(writer, "existing", Some(99), Some("bad pass"), Some(100))
                        .await?;
                    upsert_on_writer(writer, "new", Some(99), Some("bad pass"), Some(100)).await?;
                    writer
                        .execute(SqlStatement::new(
                            "INSERT INTO cursor_fixture_missing_table(value) VALUES (1)",
                            vec![],
                        ))
                        .await?;
                    Ok(Box::new(()) as Box<dyn Any + Send>)
                })
            }))
            .await;
        assert!(result.is_err(), "actual failing SQL must fail the unit");
        assert_eq!(rows(&runtime).await, before);
        assert_eq!(read(&runtime, "new").await, None);
    })
    .await;
}

#[tokio::test]
async fn missing_table_errors_are_not_empty_cursor_results() {
    bounded(async {
        let runtime = KhiveRuntime::memory().expect("isolated uninitialized runtime");
        let mut reader = runtime.sql().reader().await.unwrap();
        assert!(get(&mut *reader, "path").await.is_err());
        assert!(list(&mut *reader).await.is_err());
        drop(reader);
        let result = runtime
            .sql()
            .atomic_unit(Box::new(|writer| {
                Box::pin(async move {
                    upsert_on_writer(writer, "path", None, None, None).await?;
                    Ok(Box::new(()) as Box<dyn Any + Send>)
                })
            }))
            .await;
        assert!(result.is_err(), "write must not hide missing schema");
    })
    .await;
}

#[tokio::test]
async fn malformed_stored_values_propagate_decode_errors() {
    bounded(async {
        let runtime = fixture();
        let mut writer = runtime.sql().writer().await.unwrap();
        writer
            .execute(SqlStatement::new(
                "INSERT INTO workspace_mirror_cursor(path, last_mtime) VALUES (?1, ?2)",
                vec![
                    SqlValue::Text("malformed".into()),
                    SqlValue::Text("not an integer".into()),
                ],
            ))
            .await
            .unwrap();
        drop(writer);
        let mut reader = runtime.sql().reader().await.unwrap();
        let error = get(&mut *reader, "malformed").await.unwrap_err();
        assert!(error.to_string().contains("last_mtime"));
        assert!(
            list(&mut *reader).await.is_err(),
            "malformed row must not be silently skipped"
        );
    })
    .await;
}
