use super::*;
use crate::pool::{ConnectionPool, PoolConfig, WalCeilingPolicy};
use khive_storage::{GraphStore, WriterTaskRequestState};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
use rusqlite::types::Value;
use serde_json::json;

struct Fixture {
    store: SqlGraphStore,
    pool: Arc<ConnectionPool>,
    _directory: tempfile::TempDir,
}

impl Fixture {
    fn new(queued: bool) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: queued.then(|| directory.path().join("symmetric.db")),
                write_queue_enabled: Some(queued),
                write_routing_strict: queued,
                write_queue_capacity: 8,
                write_admission_deadline_ms: 2_000,
                wal_ceiling: WalCeilingPolicy::default(),
                disk_guard_config: Some(crate::EffectiveDiskGuardConfig {
                    reserve_bytes: 0,
                    ..Default::default()
                }),
                volume_lock_dir: Some(directory.path().join("locks")),
                ..PoolConfig::for_test()
            })
            .unwrap(),
        );
        {
            let writer = pool.writer().unwrap();
            super::super::ensure_graph_schema(writer.conn()).unwrap();
        }
        let store = SqlGraphStore::new_scoped(Arc::clone(&pool), queued, "routing-only");
        assert_eq!(pool.writer_task_handle().unwrap().is_some(), queued);
        Self {
            store,
            pool,
            _directory: directory,
        }
    }

    fn seed(&self, id: &str, namespace: &str, relation: &str, revision: i64, deleted: Option<i64>) {
        self.pool
            .writer()
            .unwrap()
            .conn()
            .execute(
                r#"INSERT INTO graph_edges
               (namespace,id,source_id,target_id,relation,weight,created_at,updated_at,
                deleted_at,metadata,target_backend)
               VALUES (?1,?2,?3,?4,?5,0.4,17,?6,?7,'{"kept":true}','remote-original')"#,
                rusqlite::params![
                    namespace,
                    id,
                    Uuid::from_u128(1).to_string(),
                    Uuid::from_u128(2).to_string(),
                    relation,
                    revision,
                    deleted
                ],
            )
            .unwrap();
    }

    fn rows(&self) -> Vec<Vec<Value>> {
        let writer = self.pool.writer().unwrap();
        let mut statement = writer
            .conn()
            .prepare(
                "SELECT namespace,id,source_id,target_id,relation,weight,created_at,updated_at, \
             deleted_at,metadata,target_backend FROM graph_edges ORDER BY namespace,id",
            )
            .unwrap();
        let rows = statement
            .query_map([], |row| (0..11).map(|column| row.get(column)).collect())
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        rows
    }
}

fn request(id: Uuid, revision: i64) -> SymmetricEdgeUpdateRequest {
    SymmetricEdgeUpdateRequest {
        namespace: "record-owner".into(),
        id: id.into(),
        source_id: Uuid::from_u128(1),
        target_id: Uuid::from_u128(2),
        relation: EdgeRelation::CompetesWith,
        weight: 0.9,
        metadata: Some(json!({"patched": true})),
        expected_updated_at_micros: revision,
        expected_deleted_at_micros: None,
    }
}

#[tokio::test]
async fn in_place_update_uses_record_namespace_and_advances_future_revision() {
    let fixture = Fixture::new(false);
    let id = Uuid::from_u128(10);
    let decoy = Uuid::from_u128(20);
    let future = chrono::Utc::now().timestamp_micros() + 60_000_000;
    fixture.seed(&id.to_string(), "record-owner", "extends", future, None);
    fixture.seed(
        &decoy.to_string(),
        "routing-only",
        "competes_with",
        100,
        Some(101),
    );
    let before = fixture.rows();
    assert!(matches!(
        fixture
            .store
            .update_symmetric_edge_if_unchanged(request(id, future))
            .await
            .unwrap(),
        SymmetricEdgeUpdateOutcome::Updated
    ));
    let after = fixture.rows();
    assert_eq!(after.len(), 2);
    assert_eq!(
        after[1], before[1],
        "routing-namespace competitor must be untouched"
    );
    let mut expected = before[0].clone();
    expected[4] = Value::Text("competes_with".into());
    expected[5] = Value::Real(0.9);
    expected[9] = Value::Text(json!({"patched": true}).to_string());
    let Value::Integer(revision) = after[0][7] else {
        panic!("integer revision")
    };
    assert!(
        revision > future,
        "revision must advance even ahead of the wall clock"
    );
    expected[7] = Value::Integer(revision);
    assert_eq!(
        after[0], expected,
        "only patch fields and revision may change"
    );
}

#[tokio::test]
async fn stale_revision_and_deletion_marker_refuse_update_and_absorption() {
    for collision in [false, true] {
        for changed_deletion in [false, true] {
            let fixture = Fixture::new(false);
            let id = Uuid::from_u128(10);
            let revision = if changed_deletion { 100 } else { 101 };
            let deleted = changed_deletion.then_some(102);
            fixture.seed(
                &id.to_string(),
                "record-owner",
                "extends",
                revision,
                deleted,
            );
            if collision {
                fixture.seed(
                    &Uuid::from_u128(20).to_string(),
                    "record-owner",
                    "competes_with",
                    90,
                    None,
                );
            }
            let before = fixture.rows();
            assert!(matches!(
                fixture
                    .store
                    .update_symmetric_edge_if_unchanged(request(id, 100))
                    .await
                    .unwrap(),
                SymmetricEdgeUpdateOutcome::Stale
            ));
            assert_eq!(
                fixture.rows(),
                before,
                "a real competitor cannot erase a stale row"
            );
            // Positive control changes only the mismatched snapshot field.
            let mut matching = request(id, revision);
            matching.expected_deleted_at_micros = deleted;
            let outcome = fixture
                .store
                .update_symmetric_edge_if_unchanged(matching)
                .await
                .unwrap();
            if collision {
                assert!(
                    matches!(outcome, SymmetricEdgeUpdateOutcome::Absorbed(ref survivor)
                    if survivor == &Uuid::from_u128(20).to_string())
                );
                assert_eq!(fixture.rows(), vec![before[1].clone()]);
            } else {
                assert!(matches!(outcome, SymmetricEdgeUpdateOutcome::Updated));
                assert_ne!(fixture.rows(), before);
            }
        }
    }
}

#[tokio::test]
async fn absorption_preserves_live_tombstoned_and_malformed_survivors() {
    for (survivor, deleted) in [
        (Uuid::from_u128(20).to_string(), None),
        (Uuid::from_u128(20).to_string(), Some(99)),
        ("not-a-uuid".to_string(), Some(99)),
    ] {
        let fixture = Fixture::new(false);
        let id = Uuid::from_u128(10);
        fixture.seed(&id.to_string(), "record-owner", "extends", 100, None);
        fixture.seed(&survivor, "record-owner", "competes_with", 90, deleted);
        let before = fixture.rows();
        assert!(
            matches!(fixture.store.update_symmetric_edge_if_unchanged(request(id, 100))
            .await.unwrap(), SymmetricEdgeUpdateOutcome::Absorbed(ref raw) if raw == &survivor)
        );
        assert_eq!(
            fixture.rows(),
            vec![before[1].clone()],
            "survivor bytes remain unchanged, even with an invalid identifier"
        );
    }
}

#[tokio::test]
async fn revision_overflow_refuses_before_any_conflict_probe() {
    for collision in [false, true] {
        let fixture = Fixture::new(false);
        let id = Uuid::from_u128(10);
        fixture.seed(&id.to_string(), "record-owner", "extends", i64::MAX, None);
        if collision {
            fixture.seed(
                &Uuid::from_u128(20).to_string(),
                "record-owner",
                "competes_with",
                90,
                None,
            );
        }
        let before = fixture.rows();
        fixture
            .pool
            .writer()
            .unwrap()
            .conn()
            .authorizer(Some(|context: AuthContext<'_>| {
                if matches!(
                    context.action,
                    AuthAction::Read {
                        table_name: "graph_edges",
                        ..
                    }
                ) {
                    Authorization::Deny
                } else {
                    Authorization::Allow
                }
            }));
        let error = fixture
            .store
            .update_symmetric_edge_if_unchanged(request(id, i64::MAX))
            .await
            .expect_err("revision must not saturate or skip the check on absorption");
        fixture
            .pool
            .writer()
            .unwrap()
            .conn()
            .authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert!(
            matches!(error, StorageError::Driver {
            capability: StorageCapability::Graph, ref operation, ref source
        } if operation == "update_edge" && matches!(source.downcast_ref::<SqliteError>(),
            Some(SqliteError::InvalidData(message)) if message.contains("i64::MAX"))),
            "overflow must win over the prohibited probe: {error:?}"
        );
        assert_eq!(fixture.rows(), before);
    }
}

#[tokio::test]
async fn failing_statement_side_effects_roll_back_on_both_writer_routes() {
    for queued in [false, true] {
        let fixture = Fixture::new(queued);
        let id = Uuid::from_u128(10);
        fixture.seed(&id.to_string(), "record-owner", "extends", 100, None);
        fixture
            .pool
            .writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TABLE mutation_side_effect (value INTEGER);
                 CREATE TRIGGER refuse_patch BEFORE UPDATE ON graph_edges BEGIN
                   INSERT INTO mutation_side_effect VALUES (1);
                   SELECT RAISE(FAIL, 'refuse patch after side effect');
                 END;",
            )
            .unwrap();
        let before = fixture.rows();
        let error = fixture
            .store
            .update_symmetric_edge_if_unchanged(request(id, 100))
            .await
            .unwrap_err();
        let driver = if queued {
            let StorageError::WriterTaskRequestFailed {
                request_state: WriterTaskRequestState::TransactionRolledBack,
                source,
            } = error
            else {
                panic!("confirmed rollback wrapper required: {error:?}")
            };
            *source
        } else {
            error
        };
        assert!(
            matches!(driver, StorageError::Driver {
            capability: StorageCapability::Graph, ref operation, ref source
        } if operation == "update_edge" && matches!(source.downcast_ref::<SqliteError>(),
            Some(SqliteError::Rusqlite(_)))),
            "preserve concrete cause: {driver:?}"
        );
        assert_eq!(fixture.rows(), before);
        let writer = fixture.pool.writer().unwrap();
        let conn = writer.conn();
        let count = || {
            conn.query_row("SELECT count(*) FROM mutation_side_effect", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
        };
        assert_eq!(
            count(),
            0,
            "the outer transaction must roll back earlier trigger writes"
        );
        // RAISE(FAIL), unlike ABORT, keeps prior statement effects in autocommit.
        // This proves that removing the outer transaction would fail the test.
        assert!(conn
            .execute("UPDATE graph_edges SET weight=0.8", [])
            .is_err());
        assert_eq!(count(), 1);
    }
}
