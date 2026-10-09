use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use khive_db::stores::note::{note_due_key_values, SqlNoteStore};
use khive_db::{ConnectionPool, PoolConfig, SqlBridge};
use khive_storage::note::{Note, NotePropertyPatch, NotePropertyPrecondition};
use khive_storage::WriterTaskRequestState;
use khive_storage::{NoteStore, SqlAccess, SqlValue, StorageCapability, StorageError};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
use serde_json::{json, Value};
use uuid::Uuid;

#[derive(Clone, Copy, Debug)]
enum Route {
    Memory,
    Standalone,
    Queued,
    StrictWithoutQueue,
}

struct Fixture {
    pool: Arc<ConnectionPool>,
    store: SqlNoteStore,
    _dir: Option<tempfile::TempDir>,
}

impl Fixture {
    fn new(route: Route) -> Self {
        let dir = (!matches!(route, Route::Memory)).then(|| tempfile::tempdir().unwrap());
        let pool = Arc::new(
            ConnectionPool::new(PoolConfig {
                path: dir.as_ref().map(|dir| dir.path().join("notes.db")),
                write_queue_enabled: Some(matches!(route, Route::Queued)),
                write_routing_strict: matches!(route, Route::StrictWithoutQueue),
                checkout_timeout: Duration::from_millis(100),
                busy_timeout: Duration::from_millis(100),
                ..PoolConfig::for_test()
            })
            .unwrap(),
        );
        pool.writer()
            .unwrap()
            .conn()
            .execute_batch(include_str!("../sql/notes-ddl.sql"))
            .unwrap();
        let store = SqlNoteStore::new(Arc::clone(&pool), dir.is_some());
        Self {
            pool,
            store,
            _dir: dir,
        }
    }

    fn insert(&self, note: &Note) {
        let (due_key, due_source) = note_due_key_values(&note.properties);
        self.pool
            .writer()
            .unwrap()
            .conn()
            .execute(
                "INSERT INTO notes \
             (id, namespace, kind, status, name, content, salience, decay_factor, \
              expires_at, properties, created_at, updated_at, deleted_at, key, \
              strict_due_key, due_source) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16)",
                rusqlite::params![
                    note.id.to_string(),
                    note.namespace,
                    note.kind,
                    note.status,
                    note.name,
                    note.content,
                    note.salience,
                    note.decay_factor,
                    note.expires_at,
                    note.properties.as_ref().map(Value::to_string),
                    note.created_at,
                    note.updated_at,
                    note.deleted_at,
                    note.key,
                    due_key,
                    due_source,
                ],
            )
            .unwrap();
    }

    fn snapshot(&self, id: Uuid) -> Vec<rusqlite::types::Value> {
        let reader = self.pool.writer().unwrap();
        let mut statement = reader
            .conn()
            .prepare("SELECT rowid, * FROM notes WHERE id = ?1")
            .unwrap();
        let columns = statement.column_count();
        statement
            .query_row([id.to_string()], |row| {
                (0..columns).map(|index| row.get(index)).collect()
            })
            .unwrap()
    }

    fn due_values(&self, id: Uuid) -> (Option<Vec<u8>>, Option<String>) {
        self.pool
            .reader()
            .unwrap()
            .query_row(
                "SELECT strict_due_key, due_source FROM notes WHERE id = ?1",
                [id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap()
    }

    async fn patch(&self, id: Uuid, patch: &NotePropertyPatch) -> Result<bool, StorageError> {
        self.store
            .try_patch_note_properties(id, "ns:test", "message", patch)
            .await
    }
}

fn note(properties: Option<Value>) -> Note {
    let mut note = Note::new("ns:test", "message", "original content");
    note.status = "archived".into();
    note.name = Some("original name".into());
    note.salience = Some(0.5);
    note.decay_factor = Some(0.25);
    note.properties = properties;
    note.created_at = 10;
    note.updated_at = 100;
    note.key = Some(note.id.to_string());
    note
}

fn patch() -> NotePropertyPatch {
    NotePropertyPatch {
        preconditions: Vec::new(),
        set: [("patched".into(), json!(true))].into(),
        extend_expires_at: None,
        updated_at: 200,
    }
}

#[tokio::test]
async fn preconditions_distinguish_absence_null_text_and_boolean() {
    let fixture = Fixture::new(Route::Memory);
    let cases = [
        (None, [false, true, true, false]),
        (Some(Value::Null), [false; 4]),
        (Some(json!(true)), [false, false, false, true]),
        (Some(json!(false)), [false; 4]),
        (Some(json!(1)), [false; 4]),
        (Some(json!(1.0)), [false; 4]),
        (Some(json!("true")), [true; 4]),
        (Some(json!("TRUE")), [false; 4]),
        (Some(json!("false")), [false; 4]),
        (Some(json!([true])), [false; 4]),
        (Some(json!({"value": true})), [false; 4]),
    ];
    for (value, expected) in cases {
        let conditions = [
            NotePropertyPrecondition::ExtractEquals {
                key: "guard".into(),
                value: SqlValue::Text("true".into()),
            },
            NotePropertyPrecondition::AbsentOrExtractEquals {
                key: "guard".into(),
                value: SqlValue::Text("true".into()),
            },
            NotePropertyPrecondition::AbsentOrTextEquals {
                key: "guard".into(),
                value: "true".into(),
            },
            NotePropertyPrecondition::TrueOrTextTrue {
                key: "guard".into(),
            },
        ];
        for (condition, expected) in conditions.into_iter().zip(expected) {
            let properties = value
                .as_ref()
                .map_or_else(|| json!({}), |value| json!({"guard": value}));
            let note = note(Some(properties));
            fixture.insert(&note);
            let before = fixture.snapshot(note.id);
            let mut patch = patch();
            patch.preconditions = vec![condition];
            assert_eq!(
                fixture.patch(note.id, &patch).await.unwrap(),
                expected,
                "{value:?}, {:?}",
                patch.preconditions
            );
            if !expected {
                assert_eq!(fixture.snapshot(note.id), before);
            }
        }
    }
}

#[tokio::test]
async fn extracted_equality_uses_sql_binding_rules_and_null_never_equals_null() {
    let fixture = Fixture::new(Route::Memory);
    let cases = [
        (json!(true), SqlValue::Bool(true), true),
        (json!(true), SqlValue::Integer(1), true),
        (json!(1), SqlValue::Bool(true), true),
        (json!(1.0), SqlValue::Integer(1), true),
        (json!("1"), SqlValue::Integer(1), false),
        (Value::Null, SqlValue::Null, false),
        (json!([1, "x"]), SqlValue::Json(json!([1, "x"])), true),
        (json!({"x": 1}), SqlValue::Json(json!({"x": 1})), true),
    ];
    for (value, bound, expected) in cases {
        for absent_or in [false, true] {
            let note = note(Some(json!({"guard": value})));
            fixture.insert(&note);
            let mut patch = patch();
            patch.preconditions = vec![if absent_or {
                NotePropertyPrecondition::AbsentOrExtractEquals {
                    key: "guard".into(),
                    value: bound.clone(),
                }
            } else {
                NotePropertyPrecondition::ExtractEquals {
                    key: "guard".into(),
                    value: bound.clone(),
                }
            }];
            let before = fixture.snapshot(note.id);
            assert_eq!(fixture.patch(note.id, &patch).await.unwrap(), expected);
            if !expected {
                assert_eq!(fixture.snapshot(note.id), before);
            }
        }
    }
}

#[tokio::test]
async fn every_row_guard_is_required_and_refusals_preserve_the_complete_row() {
    let fixture = Fixture::new(Route::Memory);
    let note = note(Some(json!({"left": "yes", "right": "no"})));
    fixture.insert(&note);
    let before = fixture.snapshot(note.id);
    for (id, namespace, kind) in [
        (Uuid::new_v4(), "ns:test", "message"),
        (note.id, "ns:other", "message"),
        (note.id, "ns:test", "observation"),
        (note.id, "NS:TEST", "message"),
        (note.id, "ns:test", "Message"),
    ] {
        assert!(!fixture
            .store
            .try_patch_note_properties(id, namespace, kind, &patch())
            .await
            .unwrap());
        assert_eq!(fixture.snapshot(note.id), before);
    }
    let mut guarded = patch();
    guarded.preconditions = vec![
        NotePropertyPrecondition::ExtractEquals {
            key: "left".into(),
            value: SqlValue::Text("yes".into()),
        },
        NotePropertyPrecondition::AbsentOrTextEquals {
            key: "right".into(),
            value: "yes".into(),
        },
    ];
    assert!(!fixture.patch(note.id, &guarded).await.unwrap());
    assert_eq!(fixture.snapshot(note.id), before);

    let mut deleted = note.clone();
    deleted.id = Uuid::new_v4();
    deleted.key = None;
    deleted.deleted_at = Some(101);
    fixture.insert(&deleted);
    let before = fixture.snapshot(deleted.id);
    assert!(!fixture.patch(deleted.id, &patch()).await.unwrap());
    assert_eq!(fixture.snapshot(deleted.id), before);
}

#[tokio::test]
async fn typed_multi_key_patch_preserves_intervening_writes_and_non_target_columns() {
    let fixture = Fixture::new(Route::Memory);
    let mut note = note(Some(json!({"guard": "ready", "keep": "original"})));
    note.expires_at = Some(999);
    fixture.insert(&note);
    let rowid = fixture.snapshot(note.id)[0].clone();
    let mut patch = patch();
    patch
        .preconditions
        .push(NotePropertyPrecondition::ExtractEquals {
            key: "guard".into(),
            value: SqlValue::Text("ready".into()),
        });
    patch.set = [
        ("text".into(), json!("hello")),
        ("integer".into(), json!(12)),
        ("real".into(), json!(2.5)),
        ("boolean".into(), json!(false)),
        ("array".into(), json!([1, "two", null])),
        ("object".into(), json!({"nested": true})),
        ("null".into(), Value::Null),
        ("literal.path[0]".into(), json!("literal key")),
        ("quote\"slash\\key".into(), json!("quoted key")),
        ("".into(), json!("empty key")),
    ]
    .into();
    fixture
        .store
        .set_note_property(note.id, "concurrent", json!({"survives": true}), 150)
        .await
        .unwrap();
    let mut expected = fixture.store.get_note(note.id).await.unwrap().unwrap();
    expected
        .properties
        .as_mut()
        .unwrap()
        .as_object_mut()
        .unwrap()
        .extend(patch.set.clone());
    expected.updated_at = 200;
    expected.version += 1;

    let observer = fixture.pool.observe_test_statement_starts(20).unwrap();
    assert!(fixture.patch(note.id, &patch).await.unwrap());
    let statements = observer.started_statements().unwrap();
    drop(observer);
    assert_eq!(
        statements
            .iter()
            .filter(|statement| statement
                .sql
                .starts_with("UPDATE notes SET properties = json_set"))
            .count(),
        1
    );
    assert!(!statements
        .iter()
        .any(|statement| statement.sql.trim_start().starts_with("SELECT")));
    assert_eq!(fixture.snapshot(note.id)[0], rowid);
    assert_eq!(
        fixture.store.get_note(note.id).await.unwrap().unwrap(),
        expected
    );
}

#[tokio::test]
async fn competing_partial_patches_preserve_both_write_sets() {
    let fixture = Fixture::new(Route::Memory);
    let note = note(Some(json!({"keep": true})));
    fixture.insert(&note);
    let mut left = patch();
    left.set = [("left_a".into(), json!(1)), ("left_b".into(), json!(2))].into();
    let mut right = patch();
    right.set = [("right_a".into(), json!(3)), ("right_b".into(), json!(4))].into();
    right.updated_at = 300;
    let (left, right) = tokio::join!(
        fixture.patch(note.id, &left),
        fixture.patch(note.id, &right)
    );
    assert!(left.unwrap());
    assert!(right.unwrap());
    let result = fixture.store.get_note(note.id).await.unwrap().unwrap();
    assert_eq!(
        result.properties,
        Some(json!({"keep": true, "left_a": 1, "left_b": 2, "right_a": 3, "right_b": 4}))
    );
    assert_eq!(result.updated_at, 300);
    assert_eq!(result.version, 3);
}

#[tokio::test]
async fn retention_and_timestamp_only_advance_and_each_match_advances_version() {
    let fixture = Fixture::new(Route::Memory);
    for current_expiry in [None, Some(100), Some(200), Some(300)] {
        for extension in [None, Some(200)] {
            for timestamp in [50, 100, 150] {
                let mut note = note(Some(json!({})));
                note.expires_at = current_expiry;
                fixture.insert(&note);
                let mut patch = patch();
                patch.extend_expires_at = extension;
                patch.updated_at = timestamp;
                assert!(fixture.patch(note.id, &patch).await.unwrap());
                let result = fixture.store.get_note(note.id).await.unwrap().unwrap();
                assert_eq!(
                    result.expires_at,
                    match (current_expiry, extension) {
                        (Some(current), Some(extension)) => Some(current.max(extension)),
                        (current, None) => current,
                        (None, extension) => extension,
                    }
                );
                assert_eq!(result.updated_at, 100.max(timestamp));
                assert_eq!(result.version, 2);
                assert!(fixture.patch(note.id, &patch).await.unwrap());
                assert_eq!(
                    fixture
                        .store
                        .get_note(note.id)
                        .await
                        .unwrap()
                        .unwrap()
                        .version,
                    3
                );
            }
        }
    }
}

#[tokio::test]
async fn sql_null_initializes_but_non_object_json_is_unchanged() {
    let fixture = Fixture::new(Route::Memory);
    let note = note(None);
    fixture.insert(&note);
    let mut patch = patch();
    patch
        .preconditions
        .push(NotePropertyPrecondition::AbsentOrTextEquals {
            key: "missing".into(),
            value: "x".into(),
        });
    assert!(fixture.patch(note.id, &patch).await.unwrap());
    assert_eq!(
        fixture
            .store
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap()
            .properties,
        Some(json!({"patched": true}))
    );
    for properties in [
        Value::Null,
        json!(true),
        json!(1),
        json!("text"),
        json!([]),
        json!([{}]),
    ] {
        let mut scalar = note.clone();
        scalar.id = Uuid::new_v4();
        scalar.key = None;
        scalar.properties = Some(properties);
        fixture.insert(&scalar);
        let before = fixture.snapshot(scalar.id);
        assert!(!fixture.patch(scalar.id, &patch).await.unwrap());
        assert_eq!(fixture.snapshot(scalar.id), before);
    }
}

#[tokio::test]
async fn literal_keys_are_used_in_preconditions_without_nested_path_interpretation() {
    let fixture = Fixture::new(Route::Memory);
    for key in [
        "a.b",
        "a[0]",
        "quote\"slash\\key",
        "",
        "line\nbreak",
        "' OR 1=1 --",
    ] {
        let note = note(Some(json!({key: "yes", "a": {"b": "no"}})));
        fixture.insert(&note);
        let mut patch = patch();
        patch
            .preconditions
            .push(NotePropertyPrecondition::ExtractEquals {
                key: key.into(),
                value: SqlValue::Text("yes".into()),
            });
        assert!(fixture.patch(note.id, &patch).await.unwrap(), "key {key:?}");
    }
}

#[tokio::test]
async fn retry_deadline_projections_follow_only_writes_to_next_attempt_at() {
    let fixture = Fixture::new(Route::Memory);
    let note = note(Some(json!({"next_attempt_at": "2026-10-09T12:00:00Z"})));
    fixture.insert(&note);
    let original_due = fixture.due_values(note.id);
    assert!(original_due.0.is_some());
    assert!(fixture.patch(note.id, &patch()).await.unwrap());
    assert_eq!(fixture.due_values(note.id), original_due);
    fixture.pool.writer().unwrap().conn().execute_batch(
        "CREATE TRIGGER reject_unrelated_due_set BEFORE UPDATE OF strict_due_key, due_source ON notes \
         BEGIN SELECT RAISE(ABORT, 'unrelated patch set due columns'); END;"
    ).unwrap();
    assert!(fixture.patch(note.id, &patch()).await.unwrap());
    fixture
        .pool
        .writer()
        .unwrap()
        .conn()
        .execute_batch("DROP TRIGGER reject_unrelated_due_set")
        .unwrap();
    for value in [
        json!("2026-10-10T09:30:00.123+02:00"),
        json!("malformed"),
        Value::Null,
        json!(17),
        json!({"x": 1}),
    ] {
        let mut patch = patch();
        patch.set.insert("next_attempt_at".into(), value.clone());
        assert!(fixture.patch(note.id, &patch).await.unwrap());
        let expected = note_due_key_values(&Some(json!({"next_attempt_at": value})));
        assert_eq!(fixture.due_values(note.id), expected);
    }
}

#[tokio::test]
async fn invalid_patch_is_rejected_before_writer_admission() {
    let fixture = Fixture::new(Route::StrictWithoutQueue);
    let note = note(Some(json!({})));
    fixture.insert(&note);
    let before = fixture.snapshot(note.id);
    let mut invalid = Vec::new();
    let mut empty = patch();
    empty.set.clear();
    invalid.push(empty);
    let mut too_many_writes = patch();
    too_many_writes.set = (0..33)
        .map(|index| (index.to_string(), json!(index)))
        .collect();
    invalid.push(too_many_writes);
    let mut too_many_guards = patch();
    too_many_guards.preconditions = (0..33)
        .map(|_| NotePropertyPrecondition::TrueOrTextTrue {
            key: "guard".into(),
        })
        .collect();
    invalid.push(too_many_guards);
    let mut nul_write = patch();
    nul_write.set.insert("short\0suffix".into(), Value::Null);
    invalid.push(nul_write);
    for condition in [
        NotePropertyPrecondition::ExtractEquals {
            key: "short\0suffix".into(),
            value: SqlValue::Null,
        },
        NotePropertyPrecondition::AbsentOrExtractEquals {
            key: "short\0suffix".into(),
            value: SqlValue::Null,
        },
        NotePropertyPrecondition::AbsentOrTextEquals {
            key: "short\0suffix".into(),
            value: "x".into(),
        },
        NotePropertyPrecondition::TrueOrTextTrue {
            key: "short\0suffix".into(),
        },
    ] {
        let mut patch = patch();
        patch.preconditions = vec![condition];
        invalid.push(patch);
    }
    for patch in invalid {
        let error = fixture.patch(note.id, &patch).await.unwrap_err();
        assert!(
            matches!(error, StorageError::InvalidInput { capability: StorageCapability::Notes, ref operation, .. } if operation == "try_patch_note_properties"),
            "{error:?}"
        );
        assert_eq!(fixture.snapshot(note.id), before);
    }
    let error = fixture.patch(note.id, &patch()).await.unwrap_err();
    assert!(
        matches!(error, StorageError::Pool { ref operation, ref message } if operation == "writer" && message.contains("strict")),
        "{error:?}"
    );
    assert_eq!(fixture.snapshot(note.id), before);
}

#[tokio::test]
async fn exactly_32_writes_and_preconditions_are_accepted() {
    let fixture = Fixture::new(Route::Memory);
    let properties: BTreeMap<_, _> = (0..32)
        .map(|index| (index.to_string(), json!(true)))
        .collect();
    let note = note(Some(json!(properties)));
    fixture.insert(&note);
    let mut patch = patch();
    patch.preconditions = properties
        .keys()
        .map(|key| NotePropertyPrecondition::TrueOrTextTrue { key: key.clone() })
        .collect();
    patch.set = properties
        .keys()
        .map(|key| (key.clone(), json!(false)))
        .collect();
    assert!(fixture.patch(note.id, &patch).await.unwrap());
    assert_eq!(
        fixture
            .store
            .get_note(note.id)
            .await
            .unwrap()
            .unwrap()
            .properties,
        Some(json!(patch.set))
    );
}

#[tokio::test]
async fn each_writer_route_preserves_statement_error_context_and_recovers() {
    for route in [Route::Memory, Route::Standalone, Route::Queued] {
        let fixture = Fixture::new(route);
        let note = note(Some(json!({})));
        fixture.insert(&note);
        fixture
            .pool
            .writer()
            .unwrap()
            .conn()
            .execute_batch(
                "CREATE TRIGGER reject_patch BEFORE UPDATE OF properties ON notes \
             BEGIN SELECT RAISE(ABORT, 'patch refused by fixture'); END;",
            )
            .unwrap();
        let before = fixture.snapshot(note.id);
        let error = fixture.patch(note.id, &patch()).await.unwrap_err();
        let source = match &error {
            StorageError::WriterTaskRequestFailed {
                request_state,
                source,
            } => {
                assert!(matches!(route, Route::Queued));
                assert_eq!(
                    *request_state,
                    WriterTaskRequestState::TransactionRolledBack
                );
                source.as_ref()
            }
            error => {
                assert!(!matches!(route, Route::Queued));
                error
            }
        };
        let expected_operation = if matches!(route, Route::Memory) {
            "pool_writer.execute"
        } else {
            "execute"
        };
        assert!(
            matches!(source, StorageError::Driver { capability: StorageCapability::Sql, operation, source } if operation == expected_operation && source.to_string().contains("patch refused by fixture")),
            "route {route:?}: {error:?}"
        );
        assert_eq!(fixture.snapshot(note.id), before);
        fixture
            .pool
            .writer()
            .unwrap()
            .conn()
            .execute_batch("DROP TRIGGER reject_patch")
            .unwrap();
        assert!(
            fixture.patch(note.id, &patch()).await.unwrap(),
            "route {route:?}"
        );
    }
}

#[tokio::test]
async fn file_fallback_uses_the_shared_standalone_slot_and_releases_it() {
    let fixture = Fixture::new(Route::Standalone);
    let note = note(Some(json!({})));
    fixture.insert(&note);
    let bridge = SqlBridge::new(Arc::clone(&fixture.pool), true);
    let held = bridge.writer().await.unwrap();
    let before = fixture.snapshot(note.id);
    let error = fixture.patch(note.id, &patch()).await.unwrap_err();
    assert!(
        matches!(error, StorageError::AdmissionTimeout { ref operation, .. } if operation == "sql_bridge.writer_handle"),
        "{error:?}"
    );
    assert_eq!(fixture.snapshot(note.id), before);
    drop(held);
    let pooled = fixture.pool.try_checkpoint_nowait().unwrap();
    assert!(
        fixture.patch(note.id, &patch()).await.unwrap(),
        "the standalone route must not wait on the held pool writer mutex"
    );
    drop(pooled);
    assert!(
        bridge.writer().await.is_ok(),
        "the method-local bridge must release its handle slot"
    );
}

fn deny_finalization(context: AuthContext<'_>) -> Authorization {
    match context.action {
        AuthAction::Transaction {
            operation: TransactionOperation::Unknown | TransactionOperation::Rollback,
        } => Authorization::Deny,
        _ => Authorization::Allow,
    }
}

#[tokio::test]
async fn queued_terminal_unknown_outcome_is_not_wrapped_or_retried() {
    let fixture = Fixture::new(Route::Queued);
    let note = note(Some(json!({})));
    fixture.insert(&note);
    let handle = fixture.pool.writer_task_handle().unwrap().unwrap();
    handle
        .send_top_level(|connection| {
            connection
                .authorizer(Some(deny_finalization))
                .map_err(|error| {
                    StorageError::driver(
                        StorageCapability::Sql,
                        "install-finalization-fault",
                        error,
                    )
                })
        })
        .await
        .unwrap();
    let error = fixture.patch(note.id, &patch()).await.unwrap_err();
    assert!(
        matches!(
            error,
            StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::SideEffectsUnknown,
                ..
            }
        ),
        "{error:?}"
    );
    let error = fixture.patch(note.id, &patch()).await.unwrap_err();
    assert!(
        matches!(
            error,
            StorageError::WriterTaskTerminated {
                request_state: WriterTaskRequestState::NotStarted,
                ..
            }
        ),
        "{error:?}"
    );
}
