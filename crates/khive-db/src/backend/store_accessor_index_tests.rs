use super::*;
use khive_storage::note::{FilterOp, Note, NoteFilter, PropertyFilter};
use khive_storage::types::{
    Direction, Edge, PageRequest, SqlValue, TraversalExecutionBudget, TraversalOptions,
    TraversalRequest,
};
use khive_storage::{StorageCapability, StorageError};
use std::time::Duration;
use uuid::Uuid;

fn attempts(backend: &StorageBackend, kind: StoreSchemaKind) -> usize {
    backend.store_schemas[kind as usize]
        .attempts
        .load(Ordering::Relaxed)
}

fn present(connection: &rusqlite::Connection, index: &str) -> bool {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name=?1)",
            [index],
            |row| row.get(0),
        )
        .unwrap()
}

fn drop_index(connection: &rusqlite::Connection, index: &str) {
    assert!(
        present(connection, index),
        "fixture must begin with actual owned index {index}"
    );
    connection
        .execute_batch(&format!("DROP INDEX {index}"))
        .unwrap();
    assert!(!present(connection, index), "external DROP must commit");
}

fn edge(source: Uuid, target: Uuid) -> Edge {
    Edge {
        id: Uuid::new_v4().into(),
        namespace: "local".into(),
        source_id: source,
        target_id: target,
        relation: khive_types::EdgeRelation::Annotates,
        weight: 1.0,
        created_at: chrono::Utc::now(),
        updated_at: chrono::Utc::now(),
        deleted_at: None,
        metadata: None,
        target_backend: None,
    }
}

async fn annotation(backend: &StorageBackend) -> (Uuid, Uuid, i64) {
    let notes = backend.notes().unwrap();
    let graph = backend.graph().unwrap();
    let target = Uuid::new_v4();
    let mut expected = None;
    for micros in [10_i64, 20, 30] {
        let mut note = Note::new("local", "observation", "receipt");
        note.properties = Some(serde_json::json!({"tags":["receipt"], "origin":"test"}));
        note.created_at = micros;
        note.updated_at = note.created_at;
        let id = note.id;
        notes.upsert_note(note).await.unwrap();
        graph.upsert_edge(edge(id, target)).await.unwrap();
        expected = Some((id, micros));
    }
    let (note, micros) = expected.unwrap();
    assert_eq!(
        graph
            .latest_annotating_note(target, "observation", "receipt")
            .await
            .unwrap(),
        Some((note, micros))
    );
    (target, note, micros)
}

fn message_filter(index: &str) -> NoteFilter {
    let mut properties = vec![
        PropertyFilter {
            json_path: "$.direction".into(),
            op: FilterOp::Eq,
            value: SqlValue::Text("inbound".into()),
        },
        PropertyFilter {
            json_path: "$.to_actor".into(),
            op: FilterOp::EqOrMissingIndexed,
            value: SqlValue::Text("lambda:reader".into()),
        },
    ];
    if index != "idx_notes_message_recipient_direction" {
        properties.push(PropertyFilter {
            json_path: "$.read".into(),
            op: FilterOp::JsonTypeNeMissing,
            value: SqlValue::Text("true".into()),
        });
    }
    if index == "idx_notes_unread_probe_recipient_type_direction" {
        properties.push(PropertyFilter {
            json_path: "$.to_actor".into(),
            op: FilterOp::JsonTypeEq,
            value: SqlValue::Text("text".into()),
        });
    }
    NoteFilter {
        kind: Some("message".into()),
        property_filters: properties,
        ..Default::default()
    }
}

async fn message(backend: &StorageBackend) -> Uuid {
    let mut note = Note::new("local", "message", "inbound");
    note.properties =
        Some(serde_json::json!({"direction":"inbound", "to_actor":"lambda:reader", "read":false}));
    let id = note.id;
    backend.notes().unwrap().upsert_note(note).await.unwrap();
    id
}

fn missing_index(
    error: &StorageError,
    capability: StorageCapability,
    operation: &str,
    index: &str,
) {
    let StorageError::Driver {
        capability: actual,
        operation: label,
        source,
    } = error
    else {
        panic!("expected unchanged SQLite Driver error: {error}");
    };
    assert_eq!(*actual, capability);
    assert_eq!(label.as_ref(), operation);
    let sqlite = source
        .downcast_ref::<rusqlite::Error>()
        .expect("original rusqlite cause");
    assert_eq!(
        sqlite.sqlite_error().unwrap().extended_code,
        rusqlite::ffi::SQLITE_ERROR
    );
    let message = match sqlite {
        rusqlite::Error::SqliteFailure(_, Some(message)) => message,
        rusqlite::Error::SqlInputError { msg, .. } => msg,
        _ => panic!("expected actual SQLite message: {sqlite}"),
    };
    assert_eq!(message, &format!("no such index: {index}"));
}

#[tokio::test]
async fn annotation_reads_repair_graph_and_notes_owned_indexes_without_reopen() {
    for (index, owner) in [
        ("idx_graph_edges_ns_tgt_rel", StoreSchemaKind::Graph),
        ("idx_graph_edges_unique_triple", StoreSchemaKind::Graph),
        ("idx_notes_created", StoreSchemaKind::Notes),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("annotation-index.db");
        let backend = StorageBackend::sqlite_for_test(&path).unwrap();
        let (target, note, micros) = annotation(&backend).await;
        let graph = backend.graph().unwrap();
        let external = rusqlite::Connection::open(&path).unwrap();
        for with_property in [false, true] {
            let graph_before = attempts(&backend, StoreSchemaKind::Graph);
            let notes_before = attempts(&backend, StoreSchemaKind::Notes);
            drop_index(&external, index);
            let result = if with_property {
                graph
                    .latest_annotating_note_with_property(
                        target,
                        "observation",
                        "receipt",
                        "origin",
                        "test",
                    )
                    .await
            } else {
                graph
                    .latest_annotating_note(target, "observation", "receipt")
                    .await
            };
            assert_eq!(result.unwrap(), Some((note, micros)), "{index}");
            assert!(present(&external, index));
            assert_eq!(
                attempts(&backend, StoreSchemaKind::Graph),
                graph_before + usize::from(matches!(owner, StoreSchemaKind::Graph))
            );
            assert_eq!(
                attempts(&backend, StoreSchemaKind::Notes),
                notes_before + usize::from(matches!(owner, StoreSchemaKind::Notes))
            );
            assert_eq!(
                backend.notes_seq_repair_run_count(),
                1,
                "index repair must not rerun notes_seq scan"
            );
        }
    }
}

#[tokio::test]
async fn message_reads_repair_each_owned_forced_index_on_every_pinned_read_route() {
    for index in [
        "idx_notes_message_recipient_direction",
        "idx_notes_unread_probe_recipient_direction",
        "idx_notes_unread_probe_recipient_type_direction",
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("message-index.db");
        let backend = StorageBackend::sqlite_for_test(&path).unwrap();
        let id = message(&backend).await;
        let notes = backend.notes().unwrap();
        let filter = message_filter(index);
        let external = rusqlite::Connection::open(&path).unwrap();
        for route in 0..5 {
            let before = attempts(&backend, StoreSchemaKind::Notes);
            drop_index(&external, index);
            match route {
                0 => {
                    let page = notes
                        .query_notes_filtered(
                            "local",
                            &filter,
                            PageRequest {
                                offset: 0,
                                limit: 10,
                            },
                        )
                        .await
                        .unwrap();
                    assert_eq!(page.total, Some(1));
                    assert_eq!(page.items[0].id, id);
                }
                1 => {
                    let page = notes
                        .query_notes_filtered_count_free(
                            "local",
                            &filter,
                            PageRequest {
                                offset: 0,
                                limit: 10,
                            },
                        )
                        .await
                        .unwrap();
                    assert_eq!(page.items.len(), 1);
                    assert_eq!(page.items[0].id, id);
                }
                2 => {
                    let rows = notes
                        .query_notes_filtered_bounded("local", &filter, 10)
                        .await
                        .unwrap();
                    assert_eq!(rows.len(), 1);
                    assert_eq!(rows[0].id, id);
                }
                3 => assert_eq!(
                    notes
                        .count_notes_filtered_in_snapshot("local", std::slice::from_ref(&filter))
                        .await
                        .unwrap(),
                    vec![1]
                ),
                _ => {
                    let counts = notes
                        .count_notes_filtered_bounded_in_snapshot(
                            "local",
                            std::slice::from_ref(&filter),
                            10,
                        )
                        .await
                        .unwrap();
                    assert_eq!(counts[0].count, 1);
                    assert!(!counts[0].saturated);
                }
            }
            assert!(present(&external, index));
            assert_eq!(attempts(&backend, StoreSchemaKind::Notes), before + 1);
            assert_eq!(backend.notes_seq_repair_run_count(), 1);
        }
    }
}

#[tokio::test]
async fn traversal_repairs_forced_indexes_before_walk_without_spending_work_budget() {
    for (direction, index) in [
        (Direction::Out, "idx_graph_edges_ns_src_rel"),
        (Direction::In, "idx_graph_edges_ns_tgt_rel"),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("traversal-index.db");
        let backend = StorageBackend::sqlite_for_test(&path).unwrap();
        let graph = backend.graph().unwrap();
        let root = Uuid::new_v4();
        let child = Uuid::new_v4();
        graph
            .upsert_edge(if direction == Direction::Out {
                edge(root, child)
            } else {
                edge(child, root)
            })
            .await
            .unwrap();
        let external = rusqlite::Connection::open(&path).unwrap();
        drop_index(&external, index);
        let budget = TraversalExecutionBudget::new(1, Duration::from_secs(5));
        let result = graph
            .traverse(TraversalRequest {
                roots: vec![root],
                options: TraversalOptions::new(1).with_direction(direction),
                include_roots: false,
                include_properties: false,
                execution_budget: budget.clone(),
            })
            .await
            .unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].nodes.len(), 1);
        assert_eq!(result[0].nodes[0].node_id, child);
        assert_eq!(
            budget.remaining_work(),
            0,
            "only the one actual adjacency row spends work"
        );
        assert!(present(&external, index));
        assert_eq!(attempts(&backend, StoreSchemaKind::Graph), 2);
    }
}

#[tokio::test]
async fn readonly_forced_index_failures_do_not_repair_or_acquire_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("readonly-index.db");
    // Rollback journal mode preserves visibility of the later external index drops.
    let writable = StorageBackend::sqlite_with_pool_config(
        &path,
        PoolConfig {
            wal_mode: false,
            ..PoolConfig::for_test()
        },
        None,
    )
    .unwrap();
    let (target, receipt, _) = annotation(&writable).await;
    message(&writable).await;
    let external = rusqlite::Connection::open(&path).unwrap();
    let journal_mode: String = external
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(journal_mode.to_ascii_lowercase(), "delete");
    let readonly = StorageBackend::sqlite_read_only_for_test(&path).unwrap();
    let graph = readonly.graph().unwrap();
    let notes = readonly.notes().unwrap();
    let before = readonly.pool.writer_acquisition_snapshot();
    drop_index(&external, "idx_graph_edges_ns_tgt_rel");
    missing_index(
        &graph
            .latest_annotating_note(target, "observation", "receipt")
            .await
            .unwrap_err(),
        StorageCapability::Graph,
        "latest_annotating_note",
        "idx_graph_edges_ns_tgt_rel",
    );
    // Every pooled reader must observe the external schema change before prepare-only reads.
    {
        let readers = (0..readonly.pool.max_readers())
            .map(|_| readonly.pool.reader().unwrap())
            .collect::<Vec<_>>();
        assert!(
            !readers.is_empty(),
            "readonly fixture needs dedicated readers"
        );
        for reader in &readers {
            let count = reader
                .query_row(
                    "SELECT COUNT(*) FROM graph_edges WHERE namespace='local'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap();
            assert_eq!(count, 3, "each reader must observe the three seeded edges");
        }
    }
    // An Out hit at limit1 ends the Both walk before its In statement.
    // Read-only behavior must not preflight the unused missing In index.
    let mut options = TraversalOptions::new(1).with_direction(Direction::Both);
    options.limit = Some(1);
    let walked = graph
        .traverse(TraversalRequest {
            roots: vec![receipt],
            options,
            include_roots: false,
            include_properties: false,
            execution_budget: TraversalExecutionBudget::new(1, Duration::from_secs(5)),
        })
        .await
        .unwrap();
    assert_eq!(walked[0].nodes[0].node_id, target);
    drop_index(&external, "idx_notes_message_recipient_direction");
    missing_index(
        &notes
            .query_notes_filtered_count_free(
                "local",
                &message_filter("idx_notes_message_recipient_direction"),
                PageRequest {
                    offset: 0,
                    limit: 10,
                },
            )
            .await
            .unwrap_err(),
        StorageCapability::Notes,
        "query_notes_filtered_count_free",
        "idx_notes_message_recipient_direction",
    );
    let after = readonly.pool.writer_acquisition_snapshot();
    assert_eq!(after.pooled_acquisitions, before.pooled_acquisitions);
    assert_eq!(attempts(&readonly, StoreSchemaKind::Graph), 0);
    assert_eq!(attempts(&readonly, StoreSchemaKind::Notes), 0);
    assert!(!present(&external, "idx_graph_edges_ns_tgt_rel"));
    assert!(!present(&external, "idx_notes_message_recipient_direction"));
}

#[tokio::test]
async fn other_sqlite_failures_keep_their_original_read_label_and_do_not_repair() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("other-sqlite-error.db");
    let backend = StorageBackend::sqlite_for_test(&path).unwrap();
    let (target, _, _) = annotation(&backend).await;
    let graph = backend.graph().unwrap();
    let external = rusqlite::Connection::open(&path).unwrap();
    external.execute_batch("DROP TABLE graph_edges").unwrap();
    let error = graph
        .latest_annotating_note(target, "observation", "receipt")
        .await
        .unwrap_err();
    let StorageError::Driver {
        capability,
        operation,
        source,
    } = error
    else {
        panic!("{error}");
    };
    assert_eq!(capability, StorageCapability::Graph);
    assert_eq!(operation.as_ref(), "latest_annotating_note");
    let sqlite = source.downcast_ref::<rusqlite::Error>().unwrap();
    let message = match sqlite {
        rusqlite::Error::SqliteFailure(_, Some(message)) => message,
        rusqlite::Error::SqlInputError { msg, .. } => msg,
        _ => panic!("expected real missing-table error: {sqlite}"),
    };
    assert_eq!(message, "no such table: graph_edges");
    assert_eq!(attempts(&backend, StoreSchemaKind::Graph), 1);
    assert_eq!(attempts(&backend, StoreSchemaKind::Notes), 1);
}

#[tokio::test]
async fn in_memory_missing_index_repair_releases_the_shared_writer_reader_lease() {
    let backend = StorageBackend::memory().unwrap();
    message(&backend).await;
    let notes = backend.notes().unwrap();
    {
        let writer = backend.pool.writer().unwrap();
        drop_index(writer.conn(), "idx_notes_message_recipient_direction");
    }
    let result = tokio::time::timeout(
        Duration::from_secs(3),
        notes.query_notes_filtered_count_free(
            "local",
            &message_filter("idx_notes_message_recipient_direction"),
            PageRequest {
                offset: 0,
                limit: 10,
            },
        ),
    )
    .await
    .expect("repair must release the shared writer lease before acquiring writer")
    .unwrap();
    assert_eq!(result.items.len(), 1);
    assert_eq!(attempts(&backend, StoreSchemaKind::Notes), 2);
}
